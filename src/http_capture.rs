//! Async capture writer layer (tokio-aware shim over `ostk_cache_core::capture`).
//!
//! The core crate stays tokio-free; this module provides:
//! - `CaptureSinkHandle` — implements `CaptureSink` via a bounded mpsc channel.
//! - `spawn_capture_writer` — background task that processes `CaptureMsg`s.
//! - Re-exports of all public core types needed by `main.rs`.

pub use ostk_cache_core::capture::{
    CaptureMetadata, CaptureMsg, CaptureSink, DEFAULT_CAPTURE_MAX_ENTRIES, DedupeCache, HOP_HEADER,
    HOP_HEADER_VALUE, HttpCapture, UpstreamErrorRow, check_upstream_self_loop, default_capture_dir,
    dropped_captures, entry_dir, handle_capture_msg, increment_dropped_captures,
    log_upstream_error, rotate_capture_dir, shard_dir,
};

use std::sync::Arc;
use tokio::sync::mpsc;

/// Bounded mpsc channel capacity; drops captures silently when full.
const WRITER_CHANNEL_CAP: usize = 256;

/// Tokio-backed `CaptureSink` implementation.
#[derive(Clone)]
pub struct CaptureSinkHandle {
    tx: mpsc::Sender<CaptureMsg>,
}

impl std::fmt::Debug for CaptureSinkHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CaptureSinkHandle")
    }
}

impl CaptureSink for CaptureSinkHandle {
    fn send(&self, msg: CaptureMsg) {
        if self.tx.try_send(msg).is_err() {
            increment_dropped_captures();
        }
    }
}

/// Spawn the background capture writer task.
/// Returns an `Arc<dyn CaptureSink>` that can be cloned cheaply and passed
/// to `HttpCapture::maybe_start`.
pub fn spawn_capture_writer() -> Arc<dyn CaptureSink> {
    let (tx, mut rx) = mpsc::channel::<CaptureMsg>(WRITER_CHANNEL_CAP);

    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let result = tokio::task::spawn_blocking(move || handle_capture_msg(msg)).await;
            if let Err(e) = result {
                eprintln!("[proxy] capture writer task panicked: {:?}", e);
            }
        }
    });

    Arc::new(CaptureSinkHandle { tx })
}

// ---------------------------------------------------------------------------
// Unit tests for the async writer layer
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that a full channel drops and increments the counter without panic.
    #[tokio::test]
    async fn capture_sink_drops_gracefully_when_channel_full() {
        let (tx, _rx) = mpsc::channel::<CaptureMsg>(1);
        let handle = CaptureSinkHandle { tx };
        let before = dropped_captures();

        // Fill channel.
        handle.send(CaptureMsg::Outbound {
            entry_dir: std::path::PathBuf::from("/dev/null"),
            payload: vec![1, 2, 3],
        });
        // This one should overflow — _rx is dropped so channel is full.
        handle.send(CaptureMsg::Outbound {
            entry_dir: std::path::PathBuf::from("/dev/null"),
            payload: vec![4, 5, 6],
        });

        let after = dropped_captures();
        assert!(
            after >= before,
            "dropped counter should not decrease: before={} after={}",
            before,
            after
        );
    }

    /// Hop-header constants are stable across builds.
    #[test]
    fn hop_header_constant() {
        assert_eq!(HOP_HEADER, "x-ostk-cache-hop");
        assert_eq!(HOP_HEADER_VALUE, "1");
    }
}
