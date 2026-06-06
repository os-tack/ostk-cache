//! Optional full HTTP body capture for provider adapters.

use serde::Serialize;
use sha2::Digest;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);

const REQUEST_IN_BODY: &str = "request-in.body";
const REQUEST_OUT_BODY: &str = "request-out.body";
const RESPONSE_BODY: &str = "response.body";
const METADATA_JSON: &str = "metadata.json";
const UPSTREAM_ERRORS_FILE: &str = "upstream-errors.jsonl";

#[derive(Debug, Clone)]
pub struct HttpCapture {
    dir: PathBuf,
    meta: CaptureMetadata,
}

#[derive(Debug, Clone, Serialize)]
struct CaptureMetadata {
    id: String,
    ts: String,
    session: String,
    method: String,
    path: String,
    status: Option<u16>,
    is_sse: Option<bool>,
    elapsed_ms: Option<u128>,
    request_headers: Vec<CapturedHeader>,
    response_headers: Vec<CapturedHeader>,
    request_in: CapturedBlob,
    request_out: Option<CapturedBlob>,
    response: Option<CapturedBlob>,
}

#[derive(Debug, Clone, Serialize)]
struct CapturedHeader {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Serialize)]
struct CapturedBlob {
    file: String,
    bytes: u64,
    sha256: String,
}

impl HttpCapture {
    pub fn maybe_start(
        enabled: bool,
        root: &Path,
        session: &str,
        method: &str,
        path: &str,
        request_headers: &http::HeaderMap,
        request_body: &[u8],
    ) -> Option<Self> {
        if !enabled {
            return None;
        }

        let id = capture_id(session, request_body);
        let dir = root.join(&id);
        match std::fs::create_dir_all(&dir)
            .and_then(|_| write_blob(&dir, REQUEST_IN_BODY, request_body))
        {
            Ok(request_in) => {
                let meta = CaptureMetadata {
                    id,
                    ts: iso8601_utc_now(),
                    session: session.to_string(),
                    method: method.to_string(),
                    path: path.to_string(),
                    status: None,
                    is_sse: None,
                    elapsed_ms: None,
                    request_headers: redact_headers(request_headers),
                    response_headers: Vec::new(),
                    request_in,
                    request_out: None,
                    response: None,
                };
                Some(Self { dir, meta })
            }
            Err(e) => {
                eprintln!("[proxy] http capture start failed: {}", e);
                None
            }
        }
    }

    /// Capture id (filename-safe). Useful for cross-referencing audit logs.
    pub fn id(&self) -> &str {
        &self.meta.id
    }

    pub fn record_outbound(&mut self, payload: &[u8]) {
        match write_blob(&self.dir, REQUEST_OUT_BODY, payload) {
            Ok(blob) => self.meta.request_out = Some(blob),
            Err(e) => eprintln!("[proxy] http capture outbound write failed: {}", e),
        }
    }

    pub fn finish(
        mut self,
        status: u16,
        is_sse: bool,
        response_headers: &http::HeaderMap,
        response_body: &[u8],
        elapsed: std::time::Duration,
    ) {
        self.meta.status = Some(status);
        self.meta.is_sse = Some(is_sse);
        self.meta.elapsed_ms = Some(elapsed.as_millis());
        self.meta.response_headers = redact_headers(response_headers);
        match write_blob(&self.dir, RESPONSE_BODY, response_body) {
            Ok(blob) => self.meta.response = Some(blob),
            Err(e) => eprintln!("[proxy] http capture response write failed: {}", e),
        }
        if let Err(e) = write_metadata(&self.dir, &self.meta) {
            eprintln!("[proxy] http capture metadata write failed: {}", e);
        }
    }
}

pub fn default_capture_dir(cwd: &Path) -> PathBuf {
    cwd.join(".ostk").join("http-capture")
}

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

fn write_blob(dir: &Path, file: &str, body: &[u8]) -> std::io::Result<CapturedBlob> {
    let path = dir.join(file);
    std::fs::write(path, body)?;
    Ok(CapturedBlob {
        file: file.to_string(),
        bytes: body.len() as u64,
        sha256: sha256_hex(body),
    })
}

fn write_metadata(dir: &Path, meta: &CaptureMetadata) -> std::io::Result<()> {
    let path = dir.join(METADATA_JSON);
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(path, bytes)
}

fn sha256_hex(body: &[u8]) -> String {
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

fn redact_headers(headers: &http::HeaderMap) -> Vec<CapturedHeader> {
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

/// Structured row written to `upstream-errors.jsonl` when an upstream provider
/// returns 4xx/5xx. The fields are deliberately small and grep-friendly so an
/// operator can spot failures without opening individual capture dirs.
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

/// Append an upstream error row to `<capture_root>/../upstream-errors.jsonl`
/// (alongside `.ostk/http-capture/`). Creates the parent directory if needed.
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
