//! Kernel-projection IPC client — Layer 2 of the rewrite (per
//! `docs/draft/ostk-cache-adapter.md` §3 and the plan at
//! `~/.claude/plans/yes-exactly-now-that-lazy-thimble.md`).
//!
//! Discovers the ostk kernel daemon's Unix-domain socket
//! (`.ostk/ostk.sock` from cwd-walk), connects, issues a single
//! `kernel/projection` JSON-RPC request, and returns the parsed
//! response.
//!
//! # Federation discipline
//!
//! Any failure here — socket missing, connect refused, parse error,
//! timeout — returns `None`. Callers MUST fall back to standalone-mode
//! synthesis. Federation is additive; the kernel daemon being down is
//! not an error condition for ostk-cache.
//!
//! # Wire format
//!
//! JSON-RPC 2.0 over Unix-domain socket, newline-delimited (matches the
//! kernel's MCP wire). One request per connection (no keepalive in
//! MVP — the connect/request/close cycle is fast and the round-trip is
//! local).

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_MS: u64 = 500;
const SOCKET_FILENAME: &str = "ostk.sock";
const OSTK_DIR: &str = ".ostk";
/// Maximum cwd-walk depth when enumerating `.ostk/ostk.sock` candidates.
const MAX_WALK_DEPTH: usize = 16;
/// Env var that pins an explicit kernel socket path, bypassing cwd-walk.
/// Use when a stale `.ostk/ostk.sock` higher up the directory tree would
/// otherwise shadow the live one at workspace level (→1839).
const SOCKET_ENV_OVERRIDE: &str = "OSTK_KERNEL_SOCKET";

/// The projection payload returned by `kernel/projection`.
///
/// →1812b: this carries both the rendered envelope (`envelope: String`) for
/// back-compat with kernels < 1812b AND the typed `projection` field for
/// kernels ≥ 1812b. Callers should prefer `projection` when present and
/// fall back to `envelope` when absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelProjection {
    /// MVP: empty string (caller appends to harness's existing system prompt).
    /// Reserved for future kernel-authored system prompts.
    #[serde(default)]
    pub system_prompt: String,
    /// Pre-rendered context message ready to drop into a synthetic user
    /// message. Includes preamble + envelope + (future) decisions/journal.
    pub context_message: String,
    /// The raw `[procs]/[loadavg]/[meminfo]/[ctx]` envelope on its own,
    /// for callers that want to compose their own context shape. Equal to
    /// `projection.joined` when both are present (byte-stability invariant
    /// asserted kernel-side).
    pub envelope: String,
    /// Typed structured projection (→1812b). `None` when talking to a
    /// kernel that hasn't been upgraded; consumers MUST handle absence by
    /// falling back to regex over `envelope` (or treating it as opaque).
    #[serde(default)]
    pub projection: Option<ostk_abi::projection::Projection>,
    /// Wire version. Bumped on breaking schema changes.
    #[serde(default = "default_wire_version")]
    pub wire_version: u32,
}

/// Re-export so consumers can `use ostk_cache::kernel_client::Projection`
/// without learning the ostk-abi path. The type is owned by ostk-abi (the
/// shared contract crate); ostk-cache merely consumes it.
pub use ostk_abi::projection::{Projection, ProjectionLine, PROJECTION_WIRE_VERSION};

fn default_wire_version() -> u32 {
    1
}

/// Enumerate kernel socket candidates in priority order:
/// 1. `$OSTK_KERNEL_SOCKET` (explicit override, returns single-element Vec).
/// 2. Cwd-walk from `start` up to `MAX_WALK_DEPTH` levels, returning every
///    `.ostk/ostk.sock` that exists on disk (closest to `start` first).
///
/// fetch_projection then connect-probes each in order until one succeeds,
/// which lets a stale socket high up the tree (e.g. legacy
/// `~/projects/.ostk/ostk.sock`) be skipped in favour of the live socket
/// at the workspace level (→1839).
pub fn enumerate_socket_candidates(start: &Path) -> Vec<PathBuf> {
    if let Ok(p) = std::env::var(SOCKET_ENV_OVERRIDE) {
        if !p.is_empty() {
            return vec![PathBuf::from(p)];
        }
    }
    let mut candidates = Vec::new();
    let mut current: Option<&Path> = Some(start);
    let mut depth = 0;
    while let Some(dir) = current {
        if depth > MAX_WALK_DEPTH {
            break;
        }
        let candidate = dir.join(OSTK_DIR).join(SOCKET_FILENAME);
        if candidate.exists() {
            candidates.push(candidate);
        }
        current = dir.parent();
        depth += 1;
    }
    candidates
}

/// Locate the ostk kernel daemon's Unix-domain socket by walking up
/// from `start` looking for `.ostk/ostk.sock`. Returns `None` if no
/// socket is found within a reasonable depth (16 levels).
///
/// Returns the first candidate from `enumerate_socket_candidates`. Kept
/// as a thin wrapper for callers that don't need the full candidate list.
pub fn find_socket(start: &Path) -> Option<PathBuf> {
    enumerate_socket_candidates(start).into_iter().next()
}

/// Fetch a fresh kernel projection over IPC.
///
/// Returns `None` on any failure mode (no socket, connect refused,
/// timeout, parse error, JSON-RPC error). Callers MUST fall back to
/// standalone synthesis. Logs failures to stderr for diagnostics.
pub async fn fetch_projection(workspace: &Path, alias: &str) -> Option<KernelProjection> {
    let candidates = enumerate_socket_candidates(workspace);
    if candidates.is_empty() {
        eprintln!(
            "[proxy] kernel_client: no ostk.sock found from {} — falling back to standalone",
            workspace.display()
        );
        return None;
    }

    let timeout_dur = Duration::from_millis(
        std::env::var("OSTK_CACHE_KERNEL_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS),
    );

    // Try each candidate in priority order. A stale socket high up the
    // tree (e.g. ~/projects/.ostk/ostk.sock) ECONNREFUSEs; we then fall
    // through to the next candidate (e.g. workspace's live socket).
    // Returns on first success. Logs every failure for diagnostics.
    let mut last_err: Option<String> = None;
    for socket_path in &candidates {
        match timeout(timeout_dur, fetch_projection_inner(socket_path, alias)).await {
            Ok(Ok(p)) => return Some(p),
            Ok(Err(e)) => {
                eprintln!(
                    "[proxy] kernel_client: tried {}: {}",
                    socket_path.display(),
                    e
                );
                last_err = Some(e);
            }
            Err(_) => {
                let msg = format!(
                    "kernel/projection timed out after {}ms",
                    timeout_dur.as_millis()
                );
                eprintln!(
                    "[proxy] kernel_client: tried {}: {}",
                    socket_path.display(),
                    msg
                );
                last_err = Some(msg);
            }
        }
    }
    if let Some(e) = last_err {
        eprintln!(
            "[proxy] kernel_client: all {} socket candidate(s) failed (last: {}); falling back to standalone",
            candidates.len(),
            e
        );
    }
    None
}

async fn fetch_projection_inner(
    socket_path: &Path,
    alias: &str,
) -> Result<KernelProjection, String> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect {}: {}", socket_path.display(), e))?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "kernel/projection",
        "params": {"alias": alias}
    });

    let mut wire = serde_json::to_vec(&request).map_err(|e| format!("serialize: {}", e))?;
    wire.push(b'\n');

    stream
        .write_all(&wire)
        .await
        .map_err(|e| format!("write: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("flush: {}", e))?;

    // Newline-delimited: read one line for the response.
    let (read_half, _write_half) = stream.split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("read: {}", e))?;
    if n == 0 {
        return Err("empty response from kernel".into());
    }

    let response: serde_json::Value =
        serde_json::from_str(&line).map_err(|e| format!("parse response: {}", e))?;

    if let Some(err) = response.get("error") {
        return Err(format!("kernel returned JSON-RPC error: {}", err));
    }

    let result = response
        .get("result")
        .ok_or_else(|| "response missing 'result' field".to_string())?;

    serde_json::from_value::<KernelProjection>(result.clone())
        .map_err(|e| format!("decode KernelProjection: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn find_socket_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        assert!(find_socket(dir.path()).is_none());
    }

    #[test]
    fn find_socket_walks_up_to_parent() {
        let root = tempdir().unwrap();
        let ostk_dir = root.path().join(".ostk");
        fs::create_dir(&ostk_dir).unwrap();
        let sock = ostk_dir.join("ostk.sock");
        fs::write(&sock, "").unwrap();

        let nested = root.path().join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();

        let found = find_socket(&nested);
        assert!(found.is_some(), "should find socket via parent walk");
        assert_eq!(found.unwrap(), sock);
    }

    #[test]
    fn find_socket_finds_in_cwd() {
        let dir = tempdir().unwrap();
        let ostk_dir = dir.path().join(".ostk");
        fs::create_dir(&ostk_dir).unwrap();
        let sock = ostk_dir.join("ostk.sock");
        fs::write(&sock, "").unwrap();

        assert_eq!(find_socket(dir.path()), Some(sock));
    }

    // ── →1839: socket enumeration + OSTK_KERNEL_SOCKET override ────────────

    /// Serialize tests that mutate process-wide environment variables so
    /// they don't race with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn enumerate_returns_all_walked_candidates_closest_first() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Save + scrub env so the override path can't influence this test.
        let saved = std::env::var(SOCKET_ENV_OVERRIDE).ok();
        unsafe { std::env::remove_var(SOCKET_ENV_OVERRIDE) };

        let root = tempdir().unwrap();
        // Sockets at root and at root/a — walking from root/a/b/c should
        // return both, with the inner one (root/a) first.
        let outer_ostk = root.path().join(".ostk");
        fs::create_dir(&outer_ostk).unwrap();
        let outer_sock = outer_ostk.join("ostk.sock");
        fs::write(&outer_sock, "").unwrap();

        let inner_dir = root.path().join("a");
        fs::create_dir(&inner_dir).unwrap();
        let inner_ostk = inner_dir.join(".ostk");
        fs::create_dir(&inner_ostk).unwrap();
        let inner_sock = inner_ostk.join("ostk.sock");
        fs::write(&inner_sock, "").unwrap();

        let nested = inner_dir.join("b").join("c");
        fs::create_dir_all(&nested).unwrap();

        let candidates = enumerate_socket_candidates(&nested);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], inner_sock, "closest candidate first");
        assert_eq!(candidates[1], outer_sock, "outer candidate second");

        if let Some(v) = saved {
            unsafe { std::env::set_var(SOCKET_ENV_OVERRIDE, v) };
        }
    }

    #[test]
    fn enumerate_honors_env_override_and_skips_walk() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var(SOCKET_ENV_OVERRIDE).ok();

        // Walk-target also has a socket — must be ignored when override set.
        let dir = tempdir().unwrap();
        let walk_ostk = dir.path().join(".ostk");
        fs::create_dir(&walk_ostk).unwrap();
        let walk_sock = walk_ostk.join("ostk.sock");
        fs::write(&walk_sock, "").unwrap();

        let pinned = PathBuf::from("/tmp/pinned-ostk-test.sock");
        unsafe { std::env::set_var(SOCKET_ENV_OVERRIDE, &pinned) };

        let candidates = enumerate_socket_candidates(dir.path());
        assert_eq!(
            candidates,
            vec![pinned],
            "env override must short-circuit cwd-walk"
        );

        unsafe { std::env::remove_var(SOCKET_ENV_OVERRIDE) };
        if let Some(v) = saved {
            unsafe { std::env::set_var(SOCKET_ENV_OVERRIDE, v) };
        }
    }

    #[test]
    fn enumerate_empty_override_falls_through_to_walk() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var(SOCKET_ENV_OVERRIDE).ok();

        let dir = tempdir().unwrap();
        let ostk = dir.path().join(".ostk");
        fs::create_dir(&ostk).unwrap();
        let sock = ostk.join("ostk.sock");
        fs::write(&sock, "").unwrap();

        unsafe { std::env::set_var(SOCKET_ENV_OVERRIDE, "") };
        let candidates = enumerate_socket_candidates(dir.path());
        assert_eq!(candidates, vec![sock], "empty override must not pin path");

        unsafe { std::env::remove_var(SOCKET_ENV_OVERRIDE) };
        if let Some(v) = saved {
            unsafe { std::env::set_var(SOCKET_ENV_OVERRIDE, v) };
        }
    }

    #[tokio::test]
    async fn fetch_projection_returns_none_when_no_socket() {
        let dir = tempdir().unwrap();
        let result = fetch_projection(dir.path(), "test").await;
        assert!(
            result.is_none(),
            "no socket should yield None (graceful fallback)"
        );
    }

    // ── →1812b: typed Projection wire decode ───────────────────────────────

    #[test]
    fn kernel_projection_decodes_typed_projection_when_present() {
        // Wire shape from a 1812b-aware kernel: includes typed `projection`
        // alongside the legacy `envelope` string.
        let json = r#"{
            "system_prompt": "",
            "context_message": "ctx",
            "envelope": "[procs] x:1\n[loadavg] y:0",
            "projection": {
                "wire_version": 1,
                "lines": [
                    {"keyword": "procs", "fields": [["x", "1"]], "rendered": "[procs] x:1"},
                    {"keyword": "loadavg", "fields": [], "rendered": "[loadavg] y:0"}
                ],
                "joined": "[procs] x:1\n[loadavg] y:0"
            },
            "wire_version": 1
        }"#;
        let kp: KernelProjection = serde_json::from_str(json).expect("typed wire decodes");
        let proj = kp.projection.expect("projection present");
        assert_eq!(proj.joined, kp.envelope, "byte-stability invariant");
        assert_eq!(proj.lines.len(), 2);
        assert_eq!(proj.line("procs").and_then(|l| l.field("x")), Some("1"));
    }

    #[test]
    fn kernel_projection_back_compat_when_typed_field_absent() {
        // Wire shape from a pre-1812b kernel: no `projection` field at all.
        // Must still decode cleanly; ostk-cache falls back to envelope:String.
        let json = r#"{
            "system_prompt": "",
            "context_message": "ctx",
            "envelope": "[procs] x:1",
            "wire_version": 1
        }"#;
        let kp: KernelProjection =
            serde_json::from_str(json).expect("legacy wire still decodes");
        assert!(
            kp.projection.is_none(),
            "legacy kernels must yield projection=None"
        );
        assert_eq!(kp.envelope, "[procs] x:1");
    }
}
