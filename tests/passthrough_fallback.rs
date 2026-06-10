//! Pass-through fallback test for the rewrite middleware (→1799).
//!
//! Two scenarios are exercised:
//!
//! 1. **Cache load fails (missing dir)**: when `OSTK_DIR` points at a
//!    directory that does not exist, `apply_rewrite` returns
//!    `RewriteOutcome::CacheLoadFailed` and the original request body
//!    is forwarded unchanged.
//! 2. **Rewrite disabled via env**: when `OSTK_REWRITE_ENABLED=0`, the
//!    rewriter short-circuits and the original body is forwarded
//!    unchanged. (Covers the operator's "kill switch" flag.)
//!
//! Both scenarios are verified at the unit level via direct calls to
//! `apply_rewrite`. The end-to-end "proxy doesn't crash" property is
//! exercised by the existing integration tests in `tests/integration.rs`
//! which run with no FileCache present.

use ostk_cache::rewrite_middleware::{RewriteConfig, RewriteOutcome, apply_rewrite};
use ostk_files_light::RewriteOptions;
use serde_json::json;

#[test]
fn missing_ostk_dir_falls_back_to_original_body() {
    let tmp = tempfile::tempdir().unwrap();
    let nonexistent = tmp.path().join("definitely-not-here");

    let config = RewriteConfig {
        enabled: true,
        ostk_dir: nonexistent.clone(),
        options: RewriteOptions::default(),
        session_id: "sess-fallback".to_string(),
    };

    let original = json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello world"}
                ]
            }
        ]
    });
    let mut body = original.clone();

    let outcome = apply_rewrite(&mut body, &config);

    match outcome {
        RewriteOutcome::CacheLoadFailed(msg) => {
            assert!(
                msg.contains("does not exist") || msg.contains(&*nonexistent.to_string_lossy()),
                "error message should mention the missing path; got: {msg}"
            );
        }
        other => panic!("expected CacheLoadFailed, got {:?}", other),
    }

    assert_eq!(
        body, original,
        "body must be unchanged on cache load failure"
    );

    // No telemetry log should have been created (the dir doesn't exist).
    let log = nonexistent.join("rewrite-events.jsonl");
    assert!(!log.exists());
}

#[test]
fn disabled_flag_falls_back_to_original_body_silently() {
    let tmp = tempfile::tempdir().unwrap();
    // Even with a valid dir, an explicit disable must short-circuit.
    let config = RewriteConfig {
        enabled: false,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions::default(),
        session_id: "sess-disabled".to_string(),
    };

    let original = json!({
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "x"}
                ]
            }
        ]
    });
    let mut body = original.clone();

    let outcome = apply_rewrite(&mut body, &config);

    assert!(matches!(outcome, RewriteOutcome::Disabled));
    assert_eq!(body, original, "disabled rewrite must not mutate body");

    // Disabled mode must NOT emit telemetry.
    let log = tmp.path().join("rewrite-events.jsonl");
    assert!(
        !log.exists(),
        "disabled rewrite must not create telemetry log"
    );
}

#[test]
fn body_with_no_rewrite_candidates_is_unchanged() {
    // No FileCache entries → every content block is a miss → no swaps.
    // Body still re-serializable but Value-equal.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path()).unwrap();

    let config = RewriteConfig {
        enabled: true,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions::default(),
        session_id: "sess-empty-cache".to_string(),
    };

    let original = json!({
        "messages": [
            {
                "role": "user",
                "content": [
                    // Above min_size_threshold but no cache hit possible
                    {"type": "text", "text": "Z".repeat(1024)}
                ]
            }
        ]
    });
    let mut body = original.clone();

    let outcome = apply_rewrite(&mut body, &config);
    match outcome {
        RewriteOutcome::Applied(report) => {
            assert_eq!(report.rewrites_applied, 0);
            assert_eq!(report.hits, 0);
            assert_eq!(report.misses, 1);
        }
        other => panic!("expected Applied, got {:?}", other),
    }
    assert_eq!(body, original, "no swaps means no body change");

    // Telemetry IS emitted on a successful (zero-swap) rewrite — the
    // operator wants to see misses too.
    let log = tmp.path().join("rewrite-events.jsonl");
    assert!(log.exists(), "successful rewrite must emit telemetry");
}
