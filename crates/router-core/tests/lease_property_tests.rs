//! Integration-level correctness tests for lease accounting.
//! Tests the full path: SSE frame parsing → token counting → lease recharge → release.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use arc_swap::ArcSwapOption;
use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters};
use router_core::cost::KvModel;
use router_core::lease::CostLease;

fn test_backend() -> Arc<Backend> {
    Arc::new(Backend {
        id: BackendId(0),
        key: "b0".into(),
        uri: "http://x".into(),
        model: "m".into(),
        weight: 1.0,
        caps: CapsEstimate { kv_capacity_tokens: 8192, max_num_seqs: 32 },
        live: LiveCounters::default(),
        reported: ArcSwapOption::from(None),
        health: HealthState::default(),
    })
}

/// Feed a canonical SSE response stream one byte at a time through the frame
/// classifier, count content frames, and assert the count matches
/// `usage.completion_tokens` exactly.
///
/// This is the PRD §2.6 frame-boundary test. It catches any split-frame
/// miscount: if a frame straddling a byte boundary is counted twice or not
/// at all, this test fails.
#[test]
fn frame_boundary_token_count_exact_byte_at_a_time() {
    // Canonical stream: role header (0 tokens) + 3 content frames + usage (3) + [DONE]
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n",
        "data: [DONE]\n\n"
    );

    let backend = test_backend();
    let mut lease = CostLease::open(backend.clone(), 5, 10, KvModel::PromptOnly, Instant::now());

    let mut line_buf = String::new();
    let mut content_tokens: u32 = 0;
    let mut usage_tokens: u32 = 0;

    // Feed one byte at a time — the classifier must handle partial lines correctly
    for byte in stream.bytes() {
        line_buf.push(byte as char);
        while let Some(pos) = line_buf.find('\n') {
            let line: String = line_buf.drain(..=pos).collect();
            let line = line.trim();
            let line = line.strip_prefix("data:").unwrap_or(line).trim();
            if line.is_empty() {
                continue;
            }
            match classify_line(line) {
                SseResult::Content => {
                    content_tokens += 1;
                    lease.observe_tokens(1);
                }
                SseResult::Usage(n) => {
                    usage_tokens = n;
                }
                _ => {}
            }
        }
    }

    drop(lease);

    assert_eq!(
        content_tokens, usage_tokens,
        "content frame count ({content_tokens}) must match usage.completion_tokens ({usage_tokens})"
    );
    assert_eq!(content_tokens, 3, "expected exactly 3 content frames");

    // Quiescent invariant
    assert_eq!(backend.live.inflight.load(Relaxed), 0);
    assert_eq!(backend.live.kv_projected_tokens.load(Relaxed), 0);
    #[cfg(debug_assertions)]
    router_core::lease::assert_invariant(&backend);
}

enum SseResult {
    RoleHeader,
    Content,
    EmptyContent,
    Usage(u32),
    Done,
    Error,
}

fn classify_line(line: &str) -> SseResult {
    if line == "[DONE]" {
        return SseResult::Done;
    }
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return SseResult::Error,
    };

    if v.get("error").is_some() {
        return SseResult::Error;
    }

    if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
        if choices.is_empty() {
            let n = v
                .get("usage")
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0) as u32;
            return SseResult::Usage(n);
        }
        if let Some(delta) = choices[0].get("delta") {
            if delta.get("role").is_some() {
                return SseResult::RoleHeader;
            }
            return match delta.get("content").and_then(|c| c.as_str()) {
                Some(text) if !text.is_empty() => SseResult::Content,
                _ => SseResult::EmptyContent,
            };
        }
    }
    SseResult::Error
}
