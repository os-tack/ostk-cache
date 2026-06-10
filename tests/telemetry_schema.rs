//! Telemetry schema test for the rewrite middleware (→1799).
//!
//! Captures one telemetry row produced by `emit_telemetry` and asserts:
//!
//! * The row parses as the documented `RewriteEventRow` schema.
//! * Every documented field is present with the expected JSON shape.
//! * `tokens_saved_estimate` is `(bytes_in - bytes_out) / 4`.
//! * The `ts` field is ISO-8601 UTC with second precision (matches
//!   `YYYY-MM-DDTHH:MM:SSZ`).
//! * Multiple writes append (don't overwrite).

use ostk_cache::rewrite_middleware::{
    RewriteConfig, RewriteEventRow, build_event_row, build_event_row_full, emit_telemetry,
    iso8601_utc_from_secs,
};
use ostk_files_light::{RewriteOptions, RewriteReport};

fn fixture_report(applied: u32, bytes_in: u64, bytes_out: u64) -> RewriteReport {
    RewriteReport {
        rewrites_applied: applied,
        bytes_in,
        bytes_out,
        hits: applied,
        misses: 0,
        skipped_stale: 0,
        skipped_below_threshold: 0,
        errors: 0,
    }
}

#[test]
fn one_row_parses_as_documented_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let config = RewriteConfig {
        enabled: true,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions::default(),
        session_id: "schema-test-session".to_string(),
    };

    let report = fixture_report(3, 18_432, 312);
    emit_telemetry(&report, &config).unwrap();

    let log = tmp.path().join("rewrite-events.jsonl");
    let raw = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 1);

    // Strict schema parse.
    let row: RewriteEventRow =
        serde_json::from_str(lines[0]).expect("row parses as RewriteEventRow");

    assert_eq!(row.session, "schema-test-session");
    assert_eq!(row.rewrites_applied, 3);
    assert_eq!(row.bytes_in, 18_432);
    assert_eq!(row.bytes_out, 312);
    assert_eq!(row.hits, 3);
    assert_eq!(row.misses, 0);
    assert_eq!(row.skipped_stale, 0);
    assert_eq!(row.skipped_below_threshold, 0);
    assert_eq!(row.errors, 0);
    assert_eq!(row.tokens_saved_estimate, (18_432 - 312) / 4);

    // Loose JSON shape — every documented field is present.
    let raw_v: serde_json::Value =
        serde_json::from_str(lines[0]).expect("row also parses as Value");
    let obj = raw_v.as_object().expect("row is JSON object");
    for key in [
        "ts",
        "session",
        "rewrites_applied",
        "bytes_in",
        "bytes_out",
        "hits",
        "misses",
        "skipped_stale",
        "skipped_below_threshold",
        "errors",
        "tokens_saved_estimate",
        // →2032 write-policy fields (always serialized; defaults = dark).
        "lane_class",
        "ttl_tier",
        "compact_target",
    ] {
        assert!(
            obj.contains_key(key),
            "schema field missing from row: {key}"
        );
    }
}

// →2032: policy telemetry fields — dark defaults, decision round-trip,
// and back-compat parse of pre-feature rows.
#[test]
fn policy_fields_dark_defaults_and_roundtrip() {
    // Dark path: no policy decision → "off"/""/None.
    let row = build_event_row(&fixture_report(0, 0, 0), "s");
    assert_eq!(row.lane_class, "off");
    assert_eq!(row.ttl_tier, "");
    assert_eq!(row.compact_target, None);

    // Active path: a DEAD decision carries class/tier/target.
    let mut lane = ostk_cache::write_policy::LaneState::default();
    let params = ostk_cache::write_policy::WritePolicyParams::default();
    let decision = ostk_cache::write_policy::decide(&mut lane, 1_000_000, 180_000, &params);
    let row = build_event_row_full(&fixture_report(1, 100, 4), "s", None, Some(&decision));
    assert_eq!(row.lane_class, decision.class.as_str());
    assert_eq!(row.ttl_tier, decision.tier.wire_str());
    assert_eq!(row.compact_target, decision.compact_target);

    // Round-trip through JSON preserves the fields.
    let json = serde_json::to_string(&row).unwrap();
    let parsed: RewriteEventRow = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, row);

    // Back-compat: a pre-→2032 row (no policy fields) parses with the
    // dark defaults.
    let legacy = r#"{"ts":"2026-05-06T12:34:56Z","session":"s","rewrites_applied":1,"bytes_in":100,"bytes_out":4,"hits":1,"misses":0,"skipped_stale":0,"skipped_below_threshold":0,"errors":0,"tokens_saved_estimate":24}"#;
    let parsed: RewriteEventRow = serde_json::from_str(legacy).unwrap();
    assert_eq!(parsed.lane_class, "off");
    assert_eq!(parsed.ttl_tier, "");
    assert_eq!(parsed.compact_target, None);
}

#[test]
fn ts_is_iso8601_utc_seconds() {
    // Test the formatter directly against a known vector.
    let ts = iso8601_utc_from_secs(1_778_070_896);
    assert_eq!(ts, "2026-05-06T12:34:56Z");

    // And via the row builder.
    let row = build_event_row(&fixture_report(0, 0, 0), "");
    let ts = row.ts;
    assert!(ts.ends_with('Z'), "ts must end with 'Z': {ts}");
    let parts: Vec<&str> = ts.split('T').collect();
    assert_eq!(parts.len(), 2, "ts must have one 'T': {ts}");
    let date = parts[0];
    let time = parts[1];

    // Date: YYYY-MM-DD
    let date_parts: Vec<&str> = date.split('-').collect();
    assert_eq!(date_parts.len(), 3, "date YYYY-MM-DD: {date}");
    assert_eq!(date_parts[0].len(), 4);
    assert_eq!(date_parts[1].len(), 2);
    assert_eq!(date_parts[2].len(), 2);

    // Time: HH:MM:SSZ — strip the trailing Z.
    let time = time.strip_suffix('Z').unwrap();
    let time_parts: Vec<&str> = time.split(':').collect();
    assert_eq!(time_parts.len(), 3, "time HH:MM:SS: {time}");
    for p in &time_parts {
        assert_eq!(p.len(), 2);
        p.parse::<u32>().expect("time component is numeric");
    }
}

#[test]
fn tokens_saved_matches_documented_formula() {
    // (bytes_in - bytes_out) / 4
    let cases = [
        (0u64, 0u64, 0u64),
        (4, 0, 1),
        (100, 4, 24),
        (4000, 100, 975),
        (10, 1000, 0), // saturates
    ];
    for (bytes_in, bytes_out, expected) in cases {
        let row = build_event_row(&fixture_report(1, bytes_in, bytes_out), "s");
        assert_eq!(
            row.tokens_saved_estimate, expected,
            "in={bytes_in} out={bytes_out}"
        );
    }
}

#[test]
fn multiple_emits_append_jsonl_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let config = RewriteConfig {
        enabled: true,
        ostk_dir: tmp.path().to_path_buf(),
        options: RewriteOptions::default(),
        session_id: "append-test".to_string(),
    };

    for i in 0..5 {
        emit_telemetry(&fixture_report(i, 1000, 100), &config).unwrap();
    }

    let log = tmp.path().join("rewrite-events.jsonl");
    let raw = std::fs::read_to_string(&log).unwrap();
    let count = raw.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(count, 5, "expected 5 appended rows, got {count}");

    // Every row parses.
    for line in raw.lines() {
        let _: RewriteEventRow = serde_json::from_str(line).unwrap();
    }
}

#[test]
fn parent_dirs_are_created_for_telemetry_log() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("a").join("b").join("c");
    let config = RewriteConfig {
        enabled: true,
        ostk_dir: nested.clone(),
        options: RewriteOptions::default(),
        session_id: "nested".to_string(),
    };

    emit_telemetry(&fixture_report(1, 100, 4), &config).unwrap();

    let log = nested.join("rewrite-events.jsonl");
    assert!(
        log.exists(),
        "telemetry should create parent dirs: {}",
        log.display()
    );
}
