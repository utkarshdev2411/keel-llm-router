use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::Request;
use router_core::features::RequestFeatures;
use router_core::tokens::TokenCounter;

use crate::length_estimator::{self, RouteHistograms};

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

/// `hists` supplies `ô` when the client did not send `max_tokens`. It is consulted,
/// never mutated, here; the feedback comes from the response stream's usage frame.
pub fn build_features(
    bytes: &Bytes,
    now: Instant,
    hists: &RouteHistograms,
    counter: &TokenCounter,
) -> Result<RequestFeatures, InboundError> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| InboundError::BadRequest(e.to_string()))?;

    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| InboundError::BadRequest("missing model".into()))?
        .to_string();

    let prompt_tokens = count_prompt_tokens(&v, counter);
    if prompt_tokens == 0 {
        return Err(InboundError::BadRequest("empty prompt".into()));
    }

    let max_tokens = v.get("max_tokens").and_then(|m| m.as_u64()).map(|n| n as u32);
    let streaming = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let model: Arc<str> = Arc::from(model.as_str());
    let route = length_estimator::route_key(&model, prompt_tokens);
    let expected_output_tokens = hists.estimate(&route, max_tokens);

    Ok(RequestFeatures {
        model,
        prompt_tokens,
        prompt_tokens_exact: counter.is_exact(),
        expected_output_tokens,
        max_tokens,
        prefix_key: None,
        streaming,
        arrived_at: now,
    })
}

/// Count the prompt using the configured counter, which must match the backend's
/// own tokenization. Concatenating message contents with a separator matters: under
/// whitespace counting, joining them bare would merge the last word of one message
/// with the first of the next and undercount by one per boundary.
fn count_prompt_tokens(v: &serde_json::Value, counter: &TokenCounter) -> u32 {
    if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
        let mut total = 0u32;
        for m in messages {
            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                total = total.saturating_add(counter.count(c));
            }
        }
        total
    } else if let Some(prompt) = v.get("prompt").and_then(|p| p.as_str()) {
        counter.count(prompt)
    } else {
        0
    }
}


/// Ensure a streaming request will carry a real completion usage frame.
///
/// Without this, `llm-d-inference-sim` (and most OpenAI-compatible engines) never
/// emit the empty-`choices` usage frame at all: every chunk carries populated
/// `choices`, `usage` stays `null` end to end, and the ONLY way to get ground-truth
/// `prompt_tokens`/`completion_tokens` is `stream_options.include_usage = true` on
/// the request. Discovered live: `router_prompt_token_ratio` and
/// `router_output_length_ratio` had never recorded a single sample against the real
/// simulator, in any run to date, despite passing every unit test -- the unit tests
/// all hand-built a usage frame that the real backend was never going to send.
///
/// This does not touch the KV recharge path, which counts content frames directly
/// via `CostLease::observe_tokens` and never depended on the usage frame.
///
/// A client's own explicit choice is never overridden: injection happens only when
/// `stream_options` (or its `include_usage` key) is entirely absent. A client that
/// requests `include_usage: false` keeps that. The one observable side effect for a
/// client that asked for neither is one extra trailing SSE frame with empty
/// `choices`, which is standard behaviour under this flag across the ecosystem
/// (this is exactly what the OpenAI API itself does when the flag is set) and every
/// mainstream client already handles it.
pub fn ensure_usage_requested(bytes: Bytes, streaming: bool) -> Bytes {
    if !streaming {
        return bytes;
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return bytes; // Malformed body: let the upstream engine reject it as-is.
    };
    let Some(obj) = v.as_object_mut() else {
        return bytes;
    };

    let already_decided = obj
        .get("stream_options")
        .and_then(|so| so.as_object())
        .is_some_and(|so| so.contains_key("include_usage"));
    if already_decided {
        return bytes;
    }

    obj.entry("stream_options")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .expect("stream_options was just inserted or already an object")
        .insert("include_usage".to_string(), serde_json::Value::Bool(true));

    match serde_json::to_vec(&v) {
        Ok(rebuilt) => Bytes::from(rebuilt),
        Err(_) => bytes, // Unreachable in practice: v parsed from valid JSON.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hists() -> RouteHistograms {
        RouteHistograms::new(300.0)
    }

    fn counter() -> TokenCounter {
        TokenCounter::new(router_core::tokens::TokenCounterKind::Whitespace, 4.0)
    }

    #[test]
    fn extracts_prompt_tokens_from_messages() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hello world"}]}"#);
        let f = build_features(&body, Instant::now(), &hists(), &counter()).unwrap();
        assert_eq!(&*f.model, "m");
        assert!(f.prompt_tokens > 0);
    }

    /// With no `max_tokens` and no history, the conservative default applies.
    #[test]
    fn falls_back_to_default_estimate_without_max_tokens_or_history() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        let f = build_features(&body, Instant::now(), &hists(), &counter()).unwrap();
        assert_eq!(f.expected_output_tokens, crate::length_estimator::DEFAULT_OUTPUT_ESTIMATE);
    }

    /// Once a route has history, it is used instead of the default. This is the
    /// wiring that was missing: the store exists but nothing consulted it.
    #[test]
    fn uses_route_history_when_max_tokens_absent() {
        let h = hists();
        let body = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        let first = build_features(&body, Instant::now(), &h, &counter()).unwrap();
        h.observe(&crate::length_estimator::route_key_for(&first), 42);

        let second = build_features(&body, Instant::now(), &h, &counter()).unwrap();
        assert_eq!(second.expected_output_tokens, 42,
            "a route with observed history must not fall back to the default");
    }

    #[test]
    fn max_tokens_becomes_expected_output() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":77}"#);
        let f = build_features(&body, Instant::now(), &hists(), &counter()).unwrap();
        assert_eq!(f.expected_output_tokens, 77);
        assert_eq!(f.max_tokens, Some(77));
    }

    /// The count must equal the backend's word count exactly, not a byte heuristic.
    #[test]
    fn prompt_tokens_match_backend_word_count() {
        let body = Bytes::from_static(
            br#"{"model":"m","messages":[{"role":"user","content":"alpha bravo charlie delta"}]}"#,
        );
        let f = build_features(&body, Instant::now(), &hists(), &counter()).unwrap();
        assert_eq!(f.prompt_tokens, 4);
        assert!(f.prompt_tokens_exact);
    }

    /// Multi-message prompts must not merge across the boundary and undercount.
    #[test]
    fn multi_message_prompts_sum_per_message() {
        let body = Bytes::from_static(
            br#"{"model":"m","messages":[{"role":"system","content":"one two"},{"role":"user","content":"three four five"}]}"#,
        );
        let f = build_features(&body, Instant::now(), &hists(), &counter()).unwrap();
        assert_eq!(f.prompt_tokens, 5);
    }

    #[test]
    fn non_streaming_requests_are_left_untouched() {
        let body = Bytes::from_static(br#"{"model":"m","stream":false,"messages":[]}"#);
        let out = ensure_usage_requested(body.clone(), false);
        assert_eq!(out, body);
    }

    /// The exact bug this exists to fix: without this injection the simulator
    /// never sends a usage frame, so prompt_tokens/completion_tokens ground truth
    /// is unobtainable and the drift audit can never fire.
    #[test]
    fn injects_include_usage_when_absent() {
        let body = Bytes::from_static(br#"{"model":"m","stream":true,"messages":[]}"#);
        let out = ensure_usage_requested(body, true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], serde_json::json!(true));
    }

    #[test]
    fn respects_client_that_explicitly_declined_usage() {
        let body = Bytes::from_static(
            br#"{"model":"m","stream":true,"stream_options":{"include_usage":false},"messages":[]}"#,
        );
        let out = ensure_usage_requested(body.clone(), true);
        assert_eq!(out, body, "an explicit client choice must not be overridden");
    }

    #[test]
    fn preserves_other_keys_already_in_stream_options() {
        let body = Bytes::from_static(
            br#"{"model":"m","stream":true,"stream_options":{"some_other_flag":true},"messages":[]}"#,
        );
        let out = ensure_usage_requested(body, true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], serde_json::json!(true));
        assert_eq!(v["stream_options"]["some_other_flag"], serde_json::json!(true));
    }

    #[test]
    fn malformed_body_passes_through_unchanged_for_upstream_to_reject() {
        let body = Bytes::from_static(b"not json");
        let out = ensure_usage_requested(body.clone(), true);
        assert_eq!(out, body);
    }

    #[test]
    fn missing_model_is_bad_request() {
        let body = Bytes::from_static(br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert!(matches!(build_features(&body, Instant::now(), &hists(), &counter()), Err(InboundError::BadRequest(_))));
    }

    #[test]
    fn empty_prompt_is_bad_request() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[]}"#);
        assert!(matches!(build_features(&body, Instant::now(), &hists(), &counter()), Err(InboundError::BadRequest(_))));
    }
}
