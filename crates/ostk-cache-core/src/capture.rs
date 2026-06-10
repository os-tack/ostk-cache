//! Optional full HTTP body capture for provider adapters.
//!
//! ## →2035 hardening
//!
//! Four changes ship in this module:
//!
//! 1. **Rotation/cap** — `capture_max_entries` (default 5000). On write, if the dir
//!    exceeds the cap, oldest entries (lexicographic order of timestamp-prefixed
//!    names) are deleted. Entries are sharded into `<root>/<8-char-prefix>/`
//!    subdirectories so no single directory holds unbounded entries. Legacy flat
//!    entries (pre-shard) are still readable; the only break is that NEW writes
//!    go into shards.
//!
//! 2. **Non-blocking capture** — `maybe_start` returns immediately; actual disk I/O
//!    is delegated to the calling crate's async writer via the `CaptureWriter` trait.
//!
//! 3. **Dedupe window** — an in-memory LRU keyed on `(session, body_sha256)` with
//!    a 60-second window. If the same key is seen within the window, the capture
//!    is skipped (counted, not written).
//!
//! 4. **Self-loop guard** — `check_upstream_self_loop` returns an error if the
//!    resolved upstream host:port equals the proxy's listen port on any local
//!    address. The hop-header ingress check lives in `main.rs`.

use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

pub const DEFAULT_CAPTURE_MAX_ENTRIES: usize = 5000;
/// Dedupe window: same (session, body_hash) within this interval is skipped.
pub const DEDUPE_WINDOW_SECS: u64 = 60;
/// Number of shard-prefix chars (first N chars of the capture id).
pub const SHARD_PREFIX_LEN: usize = 8;
/// Header name for hop detection (outbound + ingress check).
pub const HOP_HEADER: &str = "x-ostk-cache-hop";
/// Value written on outbound requests.
pub const HOP_HEADER_VALUE: &str = "1";

// ---------------------------------------------------------------------------
// Static counters
// ---------------------------------------------------------------------------

static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);
/// Captures dropped because the writer channel was full.
static DROPPED_CAPTURES: AtomicU64 = AtomicU64::new(0);

pub fn dropped_captures() -> u64 {
    DROPPED_CAPTURES.load(Ordering::Relaxed)
}

pub fn increment_dropped_captures() {
    DROPPED_CAPTURES.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// File-name constants (pub for use in the writer layer)
// ---------------------------------------------------------------------------

pub const REQUEST_IN_BODY: &str = "request-in.body";
pub const REQUEST_OUT_BODY: &str = "request-out.body";
pub const RESPONSE_BODY: &str = "response.body";
pub const METADATA_JSON: &str = "metadata.json";
const UPSTREAM_ERRORS_FILE: &str = "upstream-errors.jsonl";

// ---------------------------------------------------------------------------
// Dedupe cache
// ---------------------------------------------------------------------------

type DedupeKey = (String, String); // (session, body_sha256)

struct DedupeEntry {
    key: DedupeKey,
    seen_at: Instant,
}

/// In-memory LRU dedupe cache. Bounded by capacity; entries older than
/// `DEDUPE_WINDOW_SECS` are treated as expired.
pub struct DedupeCache {
    entries: VecDeque<DedupeEntry>,
    capacity: usize,
}

impl DedupeCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns `true` if this (session, hash) was seen within the window.
    /// Side-effect: records the key if not seen (or expired).
    pub fn check_and_record(&mut self, session: &str, body_hash: &str) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(DEDUPE_WINDOW_SECS);
        let key = (session.to_string(), body_hash.to_string());

        // Expire old entries from the front (insertion-ordered).
        while let Some(front) = self.entries.front() {
            if now.duration_since(front.seen_at) > window {
                self.entries.pop_front();
            } else {
                break;
            }
        }

        // Check if key exists anywhere in the window.
        for entry in &self.entries {
            if entry.key == key {
                return true; // duplicate
            }
        }

        // Evict oldest if at capacity.
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }

        self.entries.push_back(DedupeEntry { key, seen_at: now });
        false
    }
}

// ---------------------------------------------------------------------------
// Capture message type (used by the async writer in the main crate)
// ---------------------------------------------------------------------------

/// Messages sent to the background writer task.
pub enum CaptureMsg {
    /// Start event: write request-in.body and create the entry dir.
    Start {
        entry_dir: PathBuf,
        root: PathBuf,
        max_entries: usize,
        request_in_bytes: Vec<u8>,
        meta_partial: Box<CaptureMetadata>,
    },
    /// Outbound request body.
    Outbound {
        entry_dir: PathBuf,
        payload: Vec<u8>,
    },
    /// Finish: write response body + metadata.
    Finish {
        entry_dir: PathBuf,
        status: u16,
        is_sse: bool,
        response_headers: Vec<CapturedHeader>,
        response_body: Vec<u8>,
        elapsed_ms: u128,
    },
}

/// Process a single CaptureMsg synchronously. Call from spawn_blocking.
pub fn handle_capture_msg(msg: CaptureMsg) {
    match msg {
        CaptureMsg::Start {
            entry_dir,
            root,
            max_entries,
            request_in_bytes,
            meta_partial,
        } => {
            if let Err(e) = std::fs::create_dir_all(&entry_dir) {
                eprintln!("[proxy] capture start mkdir failed: {}", e);
                return;
            }
            if let Err(e) = write_blob_raw(&entry_dir, REQUEST_IN_BODY, &request_in_bytes) {
                eprintln!("[proxy] capture start body write failed: {}", e);
                return;
            }
            // Rotation: run after successful write (off request path).
            rotate_capture_dir(&root, max_entries);
            // Persist partial metadata so a crashed capture still has a record.
            let _ = write_metadata(&entry_dir, &meta_partial);
        }
        CaptureMsg::Outbound { entry_dir, payload } => {
            if let Err(e) = write_blob_raw(&entry_dir, REQUEST_OUT_BODY, &payload) {
                eprintln!("[proxy] capture outbound write failed: {}", e);
            }
        }
        CaptureMsg::Finish {
            entry_dir,
            status,
            is_sse,
            response_headers,
            response_body,
            elapsed_ms,
        } => {
            let resp_blob = match write_blob_raw(&entry_dir, RESPONSE_BODY, &response_body) {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("[proxy] capture response write failed: {}", e);
                    None
                }
            };
            // Update metadata: re-read partial, patch, rewrite.
            let meta_path = entry_dir.join(METADATA_JSON);
            if let Ok(raw) = std::fs::read(&meta_path) {
                if let Ok(mut meta) = serde_json::from_slice::<CaptureMetadata>(&raw) {
                    meta.status = Some(status);
                    meta.is_sse = Some(is_sse);
                    meta.elapsed_ms = Some(elapsed_ms);
                    meta.response_headers = response_headers;
                    meta.response = resp_blob;
                    let _ = write_metadata(&entry_dir, &meta);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rotation helpers
// ---------------------------------------------------------------------------

/// Count total capture entries (across shards) under `root` and delete
/// oldest by lexicographic order until count <= max_entries.
pub fn rotate_capture_dir(root: &Path, max_entries: usize) {
    if max_entries == 0 {
        return; // 0 = unlimited
    }

    // Collect all entry dirs: both sharded (<root>/<8-prefix>/<id>/)
    // and legacy flat (<root>/<id>/) by checking for metadata.json.
    let mut entries: Vec<PathBuf> = Vec::new();

    let Ok(top) = std::fs::read_dir(root) else { return };
    for top_item in top.flatten() {
        let p = top_item.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(METADATA_JSON).exists() {
            // Legacy flat entry
            entries.push(p);
        } else {
            // Potentially a shard dir — look inside.
            let Ok(shard_rd) = std::fs::read_dir(&p) else { continue };
            for shard_item in shard_rd.flatten() {
                let ep = shard_item.path();
                if ep.is_dir() {
                    entries.push(ep);
                }
            }
        }
    }

    if entries.len() <= max_entries {
        return;
    }

    // Sort by entry dir name (lexicographic == chronological for our ids).
    entries.sort_by(|a, b| {
        let a_name = a.file_name().unwrap_or_default();
        let b_name = b.file_name().unwrap_or_default();
        a_name.cmp(b_name)
    });

    let to_delete = entries.len().saturating_sub(max_entries);
    for dir in entries.iter().take(to_delete) {
        if let Err(e) = std::fs::remove_dir_all(dir) {
            eprintln!(
                "[proxy] capture rotation delete failed {}: {}",
                dir.display(),
                e
            );
        }
    }
}

/// Compute the shard subdir path for a given entry id.
pub fn shard_dir(root: &Path, id: &str) -> PathBuf {
    let prefix: String = id.chars().take(SHARD_PREFIX_LEN).collect();
    root.join(prefix)
}

/// Full entry dir path (under shard).
pub fn entry_dir(root: &Path, id: &str) -> PathBuf {
    shard_dir(root, id).join(id)
}

// ---------------------------------------------------------------------------
// Capture metadata types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureMetadata {
    pub id: String,
    pub ts: String,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: Option<u16>,
    pub is_sse: Option<bool>,
    pub elapsed_ms: Option<u128>,
    pub request_headers: Vec<CapturedHeader>,
    pub response_headers: Vec<CapturedHeader>,
    pub request_in: CapturedBlob,
    pub request_out: Option<CapturedBlob>,
    pub response: Option<CapturedBlob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedBlob {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// HttpCapture — handle passed through request handlers
// ---------------------------------------------------------------------------

/// Trait for sending capture messages; implemented in the main crate using tokio.
/// Using a trait here keeps the core crate free of the tokio dependency.
pub trait CaptureSink: Send + Sync {
    fn send(&self, msg: CaptureMsg);
}

/// Returned by `HttpCapture::maybe_start`; passed through the handler so
/// that `record_outbound` and `finish` can be called later.
/// All actual I/O is asynchronous via the internal `CaptureSink`.
#[derive(Clone)]
pub struct HttpCapture {
    id: String,
    entry_dir: PathBuf,
    sink: std::sync::Arc<dyn CaptureSink>,
}

impl std::fmt::Debug for HttpCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HttpCapture(id={})", self.id)
    }
}

impl HttpCapture {
    /// Returns `None` if capture is disabled or if the dedupe window fires.
    ///
    /// `sink` routes messages to the background writer; provided by the
    /// calling (main) crate.
    pub fn maybe_start(
        enabled: bool,
        root: &Path,
        session: &str,
        method: &str,
        path: &str,
        request_headers: &http::HeaderMap,
        request_body: &[u8],
        sink: Option<&std::sync::Arc<dyn CaptureSink>>,
        dedupe: Option<&std::sync::Arc<std::sync::Mutex<DedupeCache>>>,
        max_entries: usize,
    ) -> Option<Self> {
        if !enabled {
            return None;
        }
        let sink = sink?.clone();

        // Dedupe check.
        let body_hash = sha256_hex(request_body);
        if let Some(cache) = dedupe {
            if let Ok(mut c) = cache.lock() {
                if c.check_and_record(session, &body_hash) {
                    return None; // duplicate within window
                }
            }
        }

        let id = capture_id(session, request_body);
        let dir = entry_dir(root, &id);

        let request_in_blob = CapturedBlob {
            file: REQUEST_IN_BODY.to_string(),
            bytes: request_body.len() as u64,
            sha256: body_hash,
        };

        let meta = CaptureMetadata {
            id: id.clone(),
            ts: iso8601_utc_now(),
            session: session.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            status: None,
            is_sse: None,
            elapsed_ms: None,
            request_headers: redact_headers(request_headers),
            response_headers: Vec::new(),
            request_in: request_in_blob,
            request_out: None,
            response: None,
        };

        sink.send(CaptureMsg::Start {
            entry_dir: dir.clone(),
            root: root.to_path_buf(),
            max_entries,
            request_in_bytes: request_body.to_vec(),
            meta_partial: Box::new(meta),
        });

        Some(Self { id, entry_dir: dir, sink })
    }

    /// Capture id (filename-safe).
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn record_outbound(&mut self, payload: &[u8]) {
        self.sink.send(CaptureMsg::Outbound {
            entry_dir: self.entry_dir.clone(),
            payload: payload.to_vec(),
        });
    }

    pub fn finish(
        self,
        status: u16,
        is_sse: bool,
        response_headers: &http::HeaderMap,
        response_body: &[u8],
        elapsed: std::time::Duration,
    ) {
        self.sink.send(CaptureMsg::Finish {
            entry_dir: self.entry_dir,
            status,
            is_sse,
            response_headers: redact_headers(response_headers),
            response_body: response_body.to_vec(),
            elapsed_ms: elapsed.as_millis(),
        });
    }
}

// ---------------------------------------------------------------------------
// Startup self-loop guard
// ---------------------------------------------------------------------------

/// Fatal startup check: returns an error message if `upstream_url` resolves
/// to the proxy's own listen address.
pub fn check_upstream_self_loop(upstream_url: &str, listen_port: u16) -> Result<(), String> {
    let url = upstream_url.trim_end_matches('/');
    let without_scheme = if let Some(s) = url.strip_prefix("https://") {
        s
    } else if let Some(s) = url.strip_prefix("http://") {
        s
    } else {
        url
    };
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);

    let (host, port_str) = if let Some(colon) = host_port.rfind(':') {
        (&host_port[..colon], &host_port[colon + 1..])
    } else {
        let default_port = if url.starts_with("https://") { "443" } else { "80" };
        (host_port, default_port)
    };

    let upstream_port: u16 = match port_str.parse() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    if upstream_port != listen_port {
        return Ok(());
    }

    let local_hosts = [
        "127.0.0.1",
        "::1",
        "localhost",
        "0.0.0.0",
        "[::]",
        "[::1]",
    ];

    if local_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return Err(format!(
            "ANTHROPIC_BASE_URL / --upstream ({}) resolves to the proxy's own listen address \
             (127.0.0.1:{}). This would cause infinite request recursion (→2035). \
             Set ANTHROPIC_BASE_URL to the real upstream (e.g. https://api.anthropic.com) \
             and restart.",
            upstream_url, listen_port
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// default_capture_dir
// ---------------------------------------------------------------------------

pub fn default_capture_dir(cwd: &Path) -> PathBuf {
    cwd.join(".ostk").join("http-capture")
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn capture_id(session: &str, request_body: &[u8]) -> String {
    let seq = CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut h = sha2::Sha256::new();
    h.update(session.as_bytes());
    h.update(request_body);
    let hash = format!("{:x}", h.finalize());
    format!("{}-{:06}-{}", millis, seq, &hash[..12])
}

pub fn write_blob_raw(dir: &Path, file: &str, body: &[u8]) -> std::io::Result<CapturedBlob> {
    let path = dir.join(file);
    std::fs::write(path, body)?;
    Ok(CapturedBlob {
        file: file.to_string(),
        bytes: body.len() as u64,
        sha256: sha256_hex(body),
    })
}

pub fn write_metadata(dir: &Path, meta: &CaptureMetadata) -> std::io::Result<()> {
    let path = dir.join(METADATA_JSON);
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // Atomic replace: metadata.json is rewritten on Finish, and external
    // readers (and a killed writer) must never observe a torn/empty file.
    let tmp = dir.join(".metadata.json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

pub fn sha256_hex(body: &[u8]) -> String {
    let mut h = sha2::Sha256::new();
    h.update(body);
    format!("{:x}", h.finalize())
}

fn iso8601_utc_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = now / 86_400;
    let secs = now % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = secs / 3_600;
    let minute = (secs % 3_600) / 60;
    let second = secs % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

pub fn redact_headers(headers: &http::HeaderMap) -> Vec<CapturedHeader> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.as_str().to_ascii_lowercase();
            let value = if matches!(
                lower.as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "anthropic-api-key"
            ) {
                "[redacted]".to_string()
            } else {
                value.to_str().unwrap_or("<non-utf8>").to_string()
            };
            CapturedHeader {
                name: name.as_str().to_string(),
                value,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// UpstreamErrorRow
// ---------------------------------------------------------------------------

/// Structured row written to `upstream-errors.jsonl` when an upstream provider
/// returns 4xx/5xx.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamErrorRow {
    pub ts: String,
    pub session: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub req_bytes_in: u64,
    pub req_bytes_out: u64,
    pub resp_bytes: Option<u64>,
    pub capture_id: Option<String>,
    pub elapsed_ms: u128,
}

impl UpstreamErrorRow {
    pub fn new(
        session: &str,
        method: &str,
        path: &str,
        status: u16,
        req_bytes_in: u64,
        req_bytes_out: u64,
        elapsed: std::time::Duration,
    ) -> Self {
        Self {
            ts: iso8601_utc_now(),
            session: session.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            status,
            req_bytes_in,
            req_bytes_out,
            resp_bytes: None,
            capture_id: None,
            elapsed_ms: elapsed.as_millis(),
        }
    }

    pub fn with_capture_id(mut self, id: impl Into<String>) -> Self {
        self.capture_id = Some(id.into());
        self
    }

    pub fn with_resp_bytes(mut self, n: u64) -> Self {
        self.resp_bytes = Some(n);
        self
    }
}

/// Append an upstream error row to `<capture_root>/../upstream-errors.jsonl`.
/// Best-effort: errors are logged to stderr but never propagate.
pub fn log_upstream_error(capture_root: &Path, row: &UpstreamErrorRow) {
    let parent = capture_root.parent().unwrap_or(capture_root);
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!("[proxy] upstream-error log mkdir failed: {}", e);
        return;
    }
    let path = parent.join(UPSTREAM_ERRORS_FILE);
    let line = match serde_json::to_string(row) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[proxy] upstream-error log serialize failed: {}", e);
            return;
        }
    };
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{}", line) {
                eprintln!("[proxy] upstream-error log write failed: {}", e);
            }
        }
        Err(e) => {
            eprintln!("[proxy] upstream-error log open failed: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- Shard layout -------------------------------------------------------

    #[test]
    fn shard_path_uses_first_8_chars() {
        let root = Path::new("/tmp/captures");
        let id = "17000000000000-000001-abcdef012345";
        let expected_shard = root.join("17000000");
        let expected_entry = expected_shard.join(id);
        assert_eq!(shard_dir(root, id), expected_shard);
        assert_eq!(entry_dir(root, id), expected_entry);
    }

    #[test]
    fn shard_prefix_shorter_than_8_uses_whole_id() {
        let root = Path::new("/tmp/captures");
        let id = "short";
        assert_eq!(shard_dir(root, id), root.join("short"));
    }

    // ---- Rotation cap -------------------------------------------------------

    #[test]
    fn rotation_cap_evicts_oldest() {
        let td = TempDir::new().unwrap();
        let root = td.path();

        let ids: Vec<String> = (100u64..105)
            .map(|ts| format!("{:016}-000001-aabbccdd0011", ts))
            .collect();

        for id in &ids {
            let dir = entry_dir(root, id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("metadata.json"), b"{}").unwrap();
        }

        rotate_capture_dir(root, 3);

        let mut remaining: Vec<String> = Vec::new();
        for shard in std::fs::read_dir(root).unwrap().flatten() {
            for entry in std::fs::read_dir(shard.path()).unwrap().flatten() {
                remaining.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        remaining.sort();

        assert_eq!(remaining.len(), 3, "expected 3 entries after rotation");
        for id in &ids[2..] {
            assert!(
                remaining.iter().any(|r| r == id),
                "expected {} to survive",
                id
            );
        }
        for id in &ids[..2] {
            assert!(
                !remaining.iter().any(|r| r == id),
                "expected {} to be evicted",
                id
            );
        }
    }

    #[test]
    fn rotation_handles_legacy_flat_entries() {
        let td = TempDir::new().unwrap();
        let root = td.path();

        for i in 0u64..3 {
            let id = format!("{:016}-000001-legacy{:06}", i, i);
            let dir = root.join(&id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("metadata.json"), b"{}").unwrap();
        }

        rotate_capture_dir(root, 2);

        let remaining: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn rotation_no_op_when_under_cap() {
        let td = TempDir::new().unwrap();
        let root = td.path();

        for i in 0u64..3 {
            let id = format!("{:016}-000001-aabb{:08}", i, i);
            let dir = entry_dir(root, &id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("metadata.json"), b"{}").unwrap();
        }

        rotate_capture_dir(root, 10);

        let mut count = 0usize;
        for shard in std::fs::read_dir(root).unwrap().flatten() {
            for _ in std::fs::read_dir(shard.path()).unwrap().flatten() {
                count += 1;
            }
        }
        assert_eq!(count, 3);
    }

    // ---- Dedupe window ------------------------------------------------------

    #[test]
    fn dedupe_blocks_duplicate_within_window() {
        let mut cache = DedupeCache::new(64);
        let first = cache.check_and_record("session-1", "hash-abc");
        assert!(!first, "first occurrence should not be a duplicate");
        let second = cache.check_and_record("session-1", "hash-abc");
        assert!(second, "second occurrence within window should be duplicate");
    }

    #[test]
    fn dedupe_allows_different_session() {
        let mut cache = DedupeCache::new(64);
        cache.check_and_record("session-A", "hash-xyz");
        let different_session = cache.check_and_record("session-B", "hash-xyz");
        assert!(
            !different_session,
            "same hash different session should not be duplicate"
        );
    }

    #[test]
    fn dedupe_allows_different_hash() {
        let mut cache = DedupeCache::new(64);
        cache.check_and_record("session-1", "hash-111");
        let different_hash = cache.check_and_record("session-1", "hash-222");
        assert!(!different_hash, "different hash should not be duplicate");
    }

    #[test]
    fn dedupe_evicts_at_capacity() {
        let mut cache = DedupeCache::new(2);
        cache.check_and_record("s", "h1");
        cache.check_and_record("s", "h2");
        // This should evict h1 (oldest).
        cache.check_and_record("s", "h3");
        let is_dup = cache.check_and_record("s", "h1");
        assert!(!is_dup, "evicted entry should not appear as duplicate");
    }

    // ---- Self-loop guard ----------------------------------------------------

    #[test]
    fn self_loop_guard_rejects_loopback() {
        let result = check_upstream_self_loop("http://127.0.0.1:8080", 8080);
        assert!(result.is_err(), "should reject 127.0.0.1:proxy-port");
    }

    #[test]
    fn self_loop_guard_rejects_localhost() {
        let result = check_upstream_self_loop("http://localhost:8080", 8080);
        assert!(result.is_err(), "should reject localhost:proxy-port");
    }

    #[test]
    fn self_loop_guard_rejects_0000() {
        let result = check_upstream_self_loop("http://0.0.0.0:9000", 9000);
        assert!(result.is_err(), "should reject 0.0.0.0:proxy-port");
    }

    #[test]
    fn self_loop_guard_allows_different_port() {
        let result = check_upstream_self_loop("http://127.0.0.1:8081", 8080);
        assert!(result.is_ok(), "different port should be allowed");
    }

    #[test]
    fn self_loop_guard_allows_real_upstream() {
        let result = check_upstream_self_loop("https://api.anthropic.com", 8080);
        assert!(result.is_ok(), "real upstream should be allowed");
    }

    #[test]
    fn self_loop_guard_allows_remote_same_port() {
        let result = check_upstream_self_loop("http://example.com:8080", 8080);
        assert!(result.is_ok(), "non-local host should be allowed");
    }

    // ---- Dropped captures counter -------------------------------------------

    #[test]
    fn dropped_captures_counter_is_accessible() {
        let _ = dropped_captures();
    }

    #[test]
    fn dedupe_same_key_added_twice_is_duplicate() {
        let mut cache = DedupeCache::new(1024);
        cache.check_and_record("sess", "body-hash-aaa");
        assert!(
            cache.check_and_record("sess", "body-hash-aaa"),
            "second call with same key should be duplicate"
        );
    }
}
