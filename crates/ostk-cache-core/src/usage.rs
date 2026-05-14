#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderUsage {
    pub input_tokens: usize,
    pub cache_read_tokens: usize,
    pub cache_create_tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDialect {
    Anthropic,
    OpenAi,
}

pub fn parse_usage(dialect: UsageDialect, is_sse: bool, body: &[u8]) -> Option<ProviderUsage> {
    match (dialect, is_sse) {
        (UsageDialect::Anthropic, false) => parse_anthropic_usage_from_json(body),
        (UsageDialect::Anthropic, true) => parse_anthropic_usage_from_sse(body),
        (UsageDialect::OpenAi, false) => parse_openai_usage_from_json(body),
        (UsageDialect::OpenAi, true) => parse_openai_usage_from_sse(body),
    }
}

pub fn parse_anthropic_usage_from_json(body: &[u8]) -> Option<ProviderUsage> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = v.get("usage")?;
    Some(ProviderUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
        cache_read_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
        cache_create_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
    })
}

pub fn parse_anthropic_usage_from_sse(body: &[u8]) -> Option<ProviderUsage> {
    let text = std::str::from_utf8(body).ok()?;
    let mut current_event: Option<&str> = None;
    let mut input_tokens = 0u64;
    let mut cache_read = 0u64;
    let mut cache_create = 0u64;
    let mut found = false;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            current_event = None;
        } else if let Some(rest) = line.strip_prefix("event: ") {
            current_event = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data: ") {
            let event = current_event.unwrap_or("");
            if (event == "message_start" || event == "message_delta")
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(rest)
            {
                let usage = v
                    .get("usage")
                    .or_else(|| v.get("message").and_then(|m| m.get("usage")));
                if let Some(u) = usage {
                    if let Some(it) = u.get("input_tokens").and_then(|x| x.as_u64()) {
                        input_tokens += it;
                    }
                    if let Some(cr) = u.get("cache_read_input_tokens").and_then(|x| x.as_u64()) {
                        cache_read += cr;
                    }
                    if let Some(cc) = u
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                    {
                        cache_create += cc;
                    }
                    found = true;
                }
            }
        }
    }

    found.then_some(ProviderUsage {
        input_tokens: input_tokens as usize,
        cache_read_tokens: cache_read as usize,
        cache_create_tokens: cache_create as usize,
    })
}

pub fn parse_openai_usage_from_json(body: &[u8]) -> Option<ProviderUsage> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = v.get("usage")?;
    openai_usage_from_value(usage)
}

pub fn parse_openai_usage_from_sse(body: &[u8]) -> Option<ProviderUsage> {
    let text = std::str::from_utf8(body).ok()?;
    let mut latest = None;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data: ") else {
            continue;
        };
        if rest == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else {
            continue;
        };
        let usage = v
            .get("usage")
            .or_else(|| v.get("response").and_then(|r| r.get("usage")));
        if let Some(usage) = usage.and_then(openai_usage_from_value) {
            latest = Some(usage);
        }
    }

    latest
}

fn openai_usage_from_value(usage: &serde_json::Value) -> Option<ProviderUsage> {
    let prompt_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(|x| x.as_u64())?;
    let cached_tokens = usage
        .get("input_tokens_details")
        .or_else(|| usage.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    Some(ProviderUsage {
        input_tokens: prompt_tokens.saturating_sub(cached_tokens) as usize,
        cache_read_tokens: cached_tokens as usize,
        cache_create_tokens: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_cached_tokens_without_double_counting_prompt_total() {
        let body = br#"{
            "usage": {
                "input_tokens": 2006,
                "input_tokens_details": {"cached_tokens": 1920}
            }
        }"#;

        let usage = parse_openai_usage_from_json(body).unwrap();
        assert_eq!(usage.input_tokens, 86);
        assert_eq!(usage.cache_read_tokens, 1920);
    }
}
