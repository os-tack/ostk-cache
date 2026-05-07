//! Standalone-mode state scaffolding (per
//! `docs/draft/ostk-cache-adapter.md` §5).
//!
//! When ostk-cache runs without the ostk kernel daemon (no
//! `.ostk/ostk.sock` reachable), it implements a minimal local kernel.
//! This module owns the on-disk state for that minimal kernel:
//!
//! - **State directory**: `~/.ostk-cache/state/<project_hash>/`
//!   bootstrapped lazily on first standalone-mode request.
//! - **Journal**: `journal.jsonl` — append-only log of intercepted
//!   requests + standalone-mode activations.
//!
//! # Out of MVP scope (filed as needles under EPIC →1809)
//!
//! - Page table tier policy (LRU + recency) — see →1815.
//! - Local decisions extraction — see →1814 (persona/preferences).
//! - recall: opcode resolution against local pages — depends on the
//!   typed Projection wire (→1812) and tool-result reconstruction
//!   (→1813).
//! - True envelope synthesis from local sysinfo — currently the
//!   rebuild module emits a zeroed placeholder when no kernel
//!   envelope is available; richer local synthesis is a follow-on.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

const STATE_DIR_NAME: &str = ".ostk-cache";

/// Resolve the standalone state directory for a given workspace path.
///
/// Layout: `~/.ostk-cache/state/<project_hash>/` where `project_hash`
/// is the first 16 hex chars of `sha256(canonicalized_workspace_path)`.
/// Falls back to "unknown" if the home directory can't be resolved.
pub fn state_dir_for(workspace: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let project_hash = {
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        digest[..16].to_string()
    };
    PathBuf::from(home)
        .join(STATE_DIR_NAME)
        .join("state")
        .join(project_hash)
}

/// Ensure the state directory exists. Idempotent — safe to call on
/// every request. Returns the directory path on success, or an error
/// if the bootstrap failed (callers should log + continue, not abort).
pub fn ensure_state_dir(workspace: &Path) -> std::io::Result<PathBuf> {
    let dir = state_dir_for(workspace);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Append a single JSONL row to the standalone journal. Used to record
/// standalone-mode activations and their context for later analysis
/// (paired with the AmpRow ledger).
pub fn append_journal(workspace: &Path, row: &serde_json::Value) -> std::io::Result<()> {
    let dir = ensure_state_dir(workspace)?;
    let path = dir.join("journal.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut line = serde_json::to_string(row).unwrap_or_else(|_| "{}".into());
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Convenience: log a standalone-mode activation event with the
/// current timestamp. Failures are logged to stderr and swallowed —
/// state-side bookkeeping must not break the proxy.
pub fn log_activation(workspace: &Path, mode: &str, session: &str, turns_dropped: usize) {
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let row = serde_json::json!({
        "event": "standalone.activation",
        "ts": ts,
        "mode": mode,
        "session": session,
        "turns_dropped": turns_dropped,
    });
    if let Err(e) = append_journal(workspace, &row) {
        eprintln!("[proxy] standalone journal write failed: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Tests in this module mutate the process-global `HOME` env var,
    // which races with cargo's default parallel test execution.
    // Serialize all HOME-touching tests through this single mutex so
    // the deterministic-hash assertions don't see HOME flip mid-test.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn state_dir_is_deterministic_for_same_path() {
        let _guard = HOME_LOCK.lock().unwrap();
        let a = state_dir_for(Path::new("/some/workspace"));
        let b = state_dir_for(Path::new("/some/workspace"));
        assert_eq!(a, b);
    }

    #[test]
    fn state_dir_differs_across_workspaces() {
        let _guard = HOME_LOCK.lock().unwrap();
        let a = state_dir_for(Path::new("/workspace/a"));
        let b = state_dir_for(Path::new("/workspace/b"));
        assert_ne!(a, b);
    }

    #[test]
    fn ensure_state_dir_creates_directory() {
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        // Safety: HOME is process-global, but HOME_LOCK serializes all
        // mutating tests in this module so no concurrent reader sees a
        // half-set value.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let dir = ensure_state_dir(Path::new("/some/work")).unwrap();
        assert!(dir.exists());
        assert!(dir.is_dir());
    }

    #[test]
    fn append_journal_writes_jsonl_row() {
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let row = serde_json::json!({"test": "value"});
        append_journal(Path::new("/some/work2"), &row).unwrap();
        let dir = state_dir_for(Path::new("/some/work2"));
        let journal = dir.join("journal.jsonl");
        let contents = std::fs::read_to_string(&journal).unwrap();
        assert!(contents.contains("\"test\""));
        assert!(contents.ends_with('\n'));
    }
}
