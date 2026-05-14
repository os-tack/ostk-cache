pub const HOP_BY_HOP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    "content-type",
    "accept-encoding",
];

pub fn is_hop_by_hop_request_header(name: &http::HeaderName) -> bool {
    HOP_BY_HOP_REQUEST_HEADERS.contains(&name.as_str())
}

pub fn should_forward_response_header(name: &http::HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "transfer-encoding" | "connection" | "content-length"
    )
}

pub fn is_sse_content_type(value: &http::HeaderValue) -> bool {
    value
        .to_str()
        .unwrap_or("")
        .to_ascii_lowercase()
        .contains("text/event-stream")
}
