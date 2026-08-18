use std::sync::Arc;
use std::time::Instant;

pub struct RequestFeatures {
    pub model: Arc<str>,
    pub prompt_tokens: u32,
    pub prompt_tokens_exact: bool,
    pub expected_output_tokens: u32,
    pub max_tokens: Option<u32>,
    pub prefix_key: Option<u64>,
    pub streaming: bool,
    pub arrived_at: Instant,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RouteKey(pub Arc<str>, pub u32);
