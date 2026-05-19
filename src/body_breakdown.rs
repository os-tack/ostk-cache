//! Per-section size breakdown of an outgoing Anthropic request body.
//!
//! Diagnostic: tells us where the bytes actually live (system prompt,
//! tools schema, conversation text, tool I/O, image attachments) so we
//! can target the right layer when investigating bloat.

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct BodyBreakdown {
    pub total: usize,
    pub system: usize,
    pub tools: usize,
    pub messages_count: usize,
    pub content_text: usize,
    pub content_tool_use: usize,
    pub content_tool_result: usize,
    pub content_images: usize,
    pub image_count: usize,
    pub largest_image: usize,
}

/// Measure section sizes of a fully-rewritten request body.
///
/// Sizes are serialized JSON byte counts (what actually goes on the
/// wire). Sections aren't strictly disjoint — `total` is the whole
/// body, while per-section counters sum a subset of it (top-level keys
/// and content blocks). Use the ratios as guidance, not arithmetic.
pub fn measure_body(req: &Value) -> BodyBreakdown {
    let mut b = BodyBreakdown::default();
    b.total = json_size(req);
    b.system = req.get("system").map(json_size).unwrap_or(0);
    b.tools = req.get("tools").map(json_size).unwrap_or(0);

    if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
        b.messages_count = messages.len();
        for msg in messages {
            if let Some(content) = msg.get("content") {
                walk_content(content, &mut b);
            }
        }
    }
    b
}

fn walk_content(content: &Value, b: &mut BodyBreakdown) {
    if let Some(s) = content.as_str() {
        b.content_text += s.len();
        return;
    }
    let Some(arr) = content.as_array() else { return };
    for block in arr {
        let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let size = json_size(block);
        match kind {
            "text" => b.content_text += size,
            "tool_use" => b.content_tool_use += size,
            "tool_result" => {
                b.content_tool_result += size;
                if let Some(inner) = block.get("content") {
                    walk_tool_result_inner(inner, b);
                }
            }
            "image" | "document" => {
                b.content_images += size;
                b.image_count += 1;
                if size > b.largest_image {
                    b.largest_image = size;
                }
            }
            _ => {}
        }
    }
}

fn walk_tool_result_inner(inner: &Value, b: &mut BodyBreakdown) {
    // tool_result.content can be a string OR an array of blocks. The
    // outer tool_result size already includes everything inside it; we
    // only re-tally images so their bytes don't hide under
    // `content_tool_result` when the bloat hunt is on.
    let Some(arr) = inner.as_array() else { return };
    for block in arr {
        let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if matches!(kind, "image" | "document") {
            let size = json_size(block);
            b.content_images += size;
            b.image_count += 1;
            if size > b.largest_image {
                b.largest_image = size;
            }
        }
    }
}

fn json_size(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

impl BodyBreakdown {
    /// Verbose one-line summary suitable for `[proxy] body: ...`.
    pub fn summarize(&self) -> String {
        format!(
            "total={} sys={} tools={} msgs={} text={} tool_use={} tool_result={} images={} (n={} max={})",
            self.total,
            self.system,
            self.tools,
            self.messages_count,
            self.content_text,
            self.content_tool_use,
            self.content_tool_result,
            self.content_images,
            self.image_count,
            self.largest_image,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_body_returns_zero_sections() {
        let b = measure_body(&json!({}));
        assert_eq!(b.messages_count, 0);
        assert_eq!(b.system, 0);
        assert_eq!(b.tools, 0);
        assert_eq!(b.content_images, 0);
        assert!(b.total > 0); // "{}" is 2 bytes
    }

    #[test]
    fn counts_text_tool_use_tool_result_images() {
        let req = json!({
            "system": [{"type": "text", "text": "you are claude"}],
            "tools": [{"name": "Bash", "input_schema": {}}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "source": {"data": "AAAA"}},
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "file1\nfile2"},
                        {"type": "image", "source": {"data": "BBBB"}},
                    ]},
                ]},
            ]
        });
        let b = measure_body(&req);
        assert_eq!(b.messages_count, 3);
        assert!(b.system > 0);
        assert!(b.tools > 0);
        assert!(b.content_text > 0, "text block contributes");
        assert!(b.content_tool_use > 0, "tool_use block contributes");
        assert!(b.content_tool_result > 0, "tool_result wrapper contributes");
        assert_eq!(b.image_count, 2, "image at top level + image nested in tool_result");
        assert!(b.content_images > 0);
        assert!(b.largest_image > 0);
    }

    #[test]
    fn string_content_counts_as_text() {
        let req = json!({
            "messages": [
                {"role": "user", "content": "hello world"},
            ]
        });
        let b = measure_body(&req);
        assert_eq!(b.content_text, "hello world".len());
    }

    #[test]
    fn largest_image_tracks_max() {
        let big = "A".repeat(10_000);
        let small = "B".repeat(100);
        let req = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image", "source": {"data": small}},
                    {"type": "image", "source": {"data": big}},
                ]}
            ]
        });
        let b = measure_body(&req);
        assert_eq!(b.image_count, 2);
        assert!(b.largest_image >= 10_000);
    }

    #[test]
    fn summarize_renders_one_line() {
        let req = json!({"messages": [{"role": "user", "content": "hi"}]});
        let b = measure_body(&req);
        let s = b.summarize();
        assert!(s.contains("total="));
        assert!(s.contains("text=2"));
        assert!(!s.contains('\n'));
    }
}
