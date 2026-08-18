use serde_json::Value;

pub enum Frame<'a> {
    RoleHeader,
    Content { text: &'a str },
    EmptyContent,
    Usage { completion_tokens: u32 },
    Error { message: &'a str },
    Done,
}

pub fn parse_line(line: &str) -> Option<ParsedLine> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line == "[DONE]" {
        return Some(ParsedLine::Done);
    }
    let v: Value = serde_json::from_str(line).ok()?;
    Some(ParsedLine::Json(v))
}

pub enum ParsedLine {
    Done,
    Json(Value),
}

/// Error is checked before choices: an error frame has no `choices` key at
/// all, and a parser branching on `choices[0].delta` first never reaches it.
pub fn classify(parsed: &ParsedLine) -> Option<Frame<'_>> {
    let v = match parsed {
        ParsedLine::Done => return Some(Frame::Done),
        ParsedLine::Json(v) => v,
    };

    if let Some(err) = v.get("error") {
        let message = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
        return Some(Frame::Error { message });
    }

    let choices = v.get("choices")?.as_array()?;
    if choices.is_empty() {
        let completion_tokens = v
            .get("usage")
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;
        return Some(Frame::Usage { completion_tokens });
    }

    let delta = choices[0].get("delta")?;
    if delta.get("role").is_some() {
        return Some(Frame::RoleHeader);
    }

    match delta.get("content").and_then(|c| c.as_str()) {
        Some(text) if !text.is_empty() => Some(Frame::Content { text }),
        _ => Some(Frame::EmptyContent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_str(line: &str) -> Option<ParsedLine> {
        parse_line(line)
    }

    #[test]
    fn classifies_role_header() {
        let p = classify_str(r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#).unwrap();
        assert!(matches!(classify(&p), Some(Frame::RoleHeader)));
    }

    #[test]
    fn classifies_content_frame() {
        let p = classify_str(r#"{"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#).unwrap();
        match classify(&p) {
            Some(Frame::Content { text }) => assert_eq!(text, "Hello"),
            _ => panic!("expected content frame"),
        }
    }

    #[test]
    fn classifies_usage_frame() {
        let p = classify_str(r#"{"choices":[],"usage":{"completion_tokens":42}}"#).unwrap();
        match classify(&p) {
            Some(Frame::Usage { completion_tokens }) => assert_eq!(completion_tokens, 42),
            _ => panic!("expected usage frame"),
        }
    }

    #[test]
    fn classifies_error_frame_before_choices() {
        let p = classify_str(r#"{"error":{"message":"the kv cache does not have sufficient capacity"}}"#).unwrap();
        match classify(&p) {
            Some(Frame::Error { message }) => assert!(message.contains("kv cache")),
            _ => panic!("expected error frame"),
        }
    }

    #[test]
    fn classifies_done() {
        let p = classify_str("[DONE]").unwrap();
        assert!(matches!(classify(&p), Some(Frame::Done)));
    }

    #[test]
    fn empty_content_mid_stream_is_empty_content() {
        let p = classify_str(r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":null}]}"#).unwrap();
        assert!(matches!(classify(&p), Some(Frame::EmptyContent)));
    }

    #[test]
    fn zero_content_success_is_impossible_reconciliation() {
        let lines = [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#,
            r#"{"choices":[],"usage":{"completion_tokens":1}}"#,
            "[DONE]",
        ];
        let mut counted = 0u32;
        let mut usage_tokens = 0u32;
        let mut had_error = false;
        for l in lines {
            let p = parse_line(l).unwrap();
            match classify(&p) {
                Some(Frame::Content { .. }) => counted += 1,
                Some(Frame::Usage { completion_tokens }) => usage_tokens = completion_tokens,
                Some(Frame::Error { .. }) => had_error = true,
                _ => {}
            }
        }
        assert!(!had_error);
        assert_eq!(counted, usage_tokens);
    }
}
