//! `RewriteOptions` knobs test for the rewrite middleware (→1799).
//!
//! Verifies the `dry_run`, `min_size_threshold`, and `max_swaps`
//! options on `RewriteOptions` (re-exported from ostk-files-light)
//! behave as advertised when called through the proxy's middleware
//! adapter.

use ostk_cache::rewrite_middleware::{RewriteConfig, RewriteOutcome, apply_rewrite};
use ostk_files_light::{FileCache, FileCacheEntry, RewriteOptions};
use serde_json::json;
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn seed_entry(ostk_dir: &std::path::Path, content: &str, file_id: &str) {
    let mut cache = FileCache::load(ostk_dir);
    cache
        .put(FileCacheEntry {
            path: format!("seed-{file_id}.txt"),
            file_id: file_id.to_string(),
            uploaded_at: "2026-05-06T20:00:00Z".to_string(),
            size: content.len() as u64,
            last_seen_gen: 0,
            stale: false,
            sha256: Some(sha256_hex(content.as_bytes())),
        })
        .unwrap();
}

fn make_request(text: &str) -> serde_json::Value {
    json!({
        "messages": [
            {
                "role": "user",
                "content": [{"type": "text", "text": text}]
            }
        ]
    })
}

#[test]
fn dry_run_counts_hits_but_makes_no_swaps() {
    let tmp = tempfile::tempdir().unwrap();
    let content = "X".repeat(2048);
    seed_entry(tmp.path(), &content, "file_dry");

    let config = RewriteConfig {
        enabled: true,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions {
            dry_run: true,
            ..Default::default()
        },
        session_id: "dry".to_string(),
    };

    let original = make_request(&content);
    let mut body = original.clone();

    let outcome = apply_rewrite(&mut body, &config);
    match outcome {
        RewriteOutcome::Applied(report) => {
            assert_eq!(report.hits, 1, "dry-run still counts hits");
            assert_eq!(
                report.rewrites_applied, 0,
                "dry-run must not apply any swaps"
            );
        }
        other => panic!("expected Applied, got {:?}", other),
    }

    assert_eq!(body, original, "dry-run must not mutate the body");
}

#[test]
fn min_size_threshold_skips_small_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    // Content below the explicit threshold.
    let content = "Y".repeat(64);
    seed_entry(tmp.path(), &content, "file_small");

    let config = RewriteConfig {
        enabled: true,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions {
            min_size_threshold: 256,
            ..Default::default()
        },
        session_id: "thresh".to_string(),
    };

    let mut body = make_request(&content);

    let outcome = apply_rewrite(&mut body, &config);
    match outcome {
        RewriteOutcome::Applied(report) => {
            assert_eq!(report.skipped_below_threshold, 1);
            assert_eq!(report.rewrites_applied, 0);
            assert_eq!(report.hits, 0);
        }
        other => panic!("expected Applied, got {:?}", other),
    }
}

#[test]
fn max_swaps_caps_applied_count() {
    let tmp = tempfile::tempdir().unwrap();
    let content_a = "A".repeat(2048);
    let content_b = "B".repeat(2048);
    let content_c = "C".repeat(2048);
    seed_entry(tmp.path(), &content_a, "file_a");
    seed_entry(tmp.path(), &content_b, "file_b");
    seed_entry(tmp.path(), &content_c, "file_c");

    let config = RewriteConfig {
        enabled: true,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions {
            max_swaps: 2,
            ..Default::default()
        },
        session_id: "cap".to_string(),
    };

    let mut body = json!({
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": content_a},
                    {"type": "text", "text": content_b},
                    {"type": "text", "text": content_c}
                ]
            }
        ]
    });

    let outcome = apply_rewrite(&mut body, &config);
    match outcome {
        RewriteOutcome::Applied(report) => {
            assert_eq!(
                report.rewrites_applied, 2,
                "max_swaps caps applied at 2 (got {}); report: {:?}",
                report.rewrites_applied, report
            );
            assert_eq!(report.hits, 2, "third match falls under the cap");
        }
        other => panic!("expected Applied, got {:?}", other),
    }
}

#[test]
fn defaults_are_min_size_512_and_no_dry_run() {
    let opts = RewriteOptions::default();
    assert_eq!(opts.min_size_threshold, 512);
    assert!(!opts.dry_run);
    assert_eq!(opts.max_swaps, 0);
}
