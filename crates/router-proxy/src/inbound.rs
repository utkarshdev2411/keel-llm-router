use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::Request;
use router_core::features::RequestFeatures;

#[derive(thiserror::Error, Debug)]
pub enum InboundError {
    #[error("request body exceeded cap")]
    BodyTooLarge,
    #[error("malformed request body: {0}")]
    BadRequest(String),
}

pub async fn read_body(req: Request<Incoming>, max_bytes: usize) -> Result<(hyper::http::request::Parts, Bytes), InboundError> {
    let (parts, body) = req.into_parts();
    let limited = Limited::new(body, max_bytes);
    let collected = limited
        .collect()
        .await
        .map_err(|_| InboundError::BodyTooLarge)?;
    Ok((parts, collected.to_bytes()))
}

pub fn build_features(bytes: &Bytes, now: Instant) -> Result<RequestFeatures, InboundError> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| InboundError::BadRequest(e.to_string()))?;

    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| InboundError::BadRequest("missing model".into()))?
        .to_string();

    let prompt_tokens = estimate_prompt_tokens(&v);
    if prompt_tokens == 0 {
        return Err(InboundError::BadRequest("empty prompt".into()));
    }

    let max_tokens = v.get("max_tokens").and_then(|m| m.as_u64()).map(|n| n as u32);
    let streaming = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let expected_output_tokens = max_tokens.unwrap_or(DEFAULT_OUTPUT_ESTIMATE);

    Ok(RequestFeatures {
        model: Arc::from(model.as_str()),
        prompt_tokens,
        prompt_tokens_exact: false,
        expected_output_tokens,
        max_tokens,
        prefix_key: None,
        streaming,
        arrived_at: now,
    })
}

const DEFAULT_OUTPUT_ESTIMATE: u32 = 128;

/// Byte-length estimate, not an exact tokenizer count. Phase 1 keeps this
/// simple; F4 allows swapping in an exact `tokenizers` count later without
/// changing this function's signature.
fn estimate_prompt_tokens(v: &serde_json::Value) -> u32 {
    let mut chars = 0usize;
    if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
        for m in messages {
            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                chars += c.len();
            }
        }
    } else if let Some(prompt) = v.get("prompt").and_then(|p| p.as_str()) {
        chars += prompt.len();
    }
    ((chars as f64) / 4.0).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_prompt_tokens_from_messages() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hello world"}]}"#);
        let f = build_features(&body, Instant::now()).unwrap();
        assert_eq!(&*f.model, "m");
        assert!(f.prompt_tokens > 0);
    }

    #[test]
    fn max_tokens_becomes_expected_output() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":77}"#);
        let f = build_features(&body, Instant::now()).unwrap();
        assert_eq!(f.expected_output_tokens, 77);
        assert_eq!(f.max_tokens, Some(77));
    }

    #[test]
    fn missing_model_is_bad_request() {
        let body = Bytes::from_static(br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert!(matches!(build_features(&body, Instant::now()), Err(InboundError::BadRequest(_))));
    }

    #[test]
    fn empty_prompt_is_bad_request() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[]}"#);
        assert!(matches!(build_features(&body, Instant::now()), Err(InboundError::BadRequest(_))));
    }
}
