use ostk_cache_core::http::is_sse_content_type;

#[test]
fn test_sse_content_type() {
    assert!(is_sse_content_type(&axum::http::HeaderValue::from_static("text/event-stream")));
    assert!(is_sse_content_type(&axum::http::HeaderValue::from_static("text/event-stream; charset=utf-8")));
    assert!(!is_sse_content_type(&axum::http::HeaderValue::from_static("application/json")));
}
