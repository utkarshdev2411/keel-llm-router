//! Prometheus text format parser for vLLM backend metrics.
//!
//! Parses the Prometheus exposition format from backend `/metrics` endpoints
//! and extracts metrics needed for signal plane operation: KV usage, request
//! counts, prefix cache statistics, and capacity information.
//!
//! Key invariants:
//! - Absent metrics return `None`, never zero
//! - `kv_cache_usage_perc` is a fraction 0.0-1.0, not a percentage 0-100
//! - Metric names are matched by prefix + delimiter to avoid substring collisions
//! - Malformed lines are skipped without panicking
//! - Counter deltas are computed from previous samples for windowed statistics

use router_core::backend::ReportedLoad;
use std::time::Instant;

/// Cumulative counters carried between scrapes to compute windowed deltas.
#[derive(Copy, Clone, Debug, Default)]
pub struct CounterSample {
    pub prefix_hits: Option<f64>,
    pub prefix_queries: Option<f64>,
}

/// Backend-reported capacity for startup assertion.
#[derive(Debug, Clone)]
pub struct ReportedCapacity {
    pub block_size: u32,
    pub num_gpu_blocks: u32,
}

/// Extract a numeric gauge/counter by metric name, ignoring label block.
///
/// Returns `None` when metric is absent — NEVER `Some(0.0)`.
///
/// Matches "name{" or "name " to prevent substring collisions.
/// E.g., "vllm:prefix_cache_hits" does not match "vllm:external_prefix_cache_hits".
///
/// # Examples
/// ```ignore
/// let text = "vllm:num_requests_running{model_name=\"test\"} 7\n";
/// assert_eq!(metric_value(text, "vllm:num_requests_running"), Some(7.0));
/// ```
pub fn metric_value(text: &str, name: &str) -> Option<f64> {
    for line in text.lines() {
        // Skip comments, HELP, and TYPE lines
        if line.trim_start().starts_with('#') {
            continue;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Match metric name followed by '{' (with labels) or whitespace (bare metric)
        // This prevents substring matches like "prefix_cache_hits" matching "external_prefix_cache_hits"
        if let Some(after_name) = line.strip_prefix(name) {
            // Check if followed by '{' or whitespace
            if let Some(first_char) = after_name.chars().next() {
                if first_char == '{' || first_char.is_whitespace() {
                    // Extract the value part
                    let value_str = if let Some(brace_pos) = after_name.find('{') {
                        // Has labels: find closing brace and extract value after it
                        if let Some(close_brace) = after_name[brace_pos..].find('}') {
                            after_name[brace_pos + close_brace + 1..].trim()
                        } else {
                            continue; // Malformed, skip
                        }
                    } else {
                        // Bare metric: value is directly after name
                        after_name.trim()
                    };

                    // Parse the numeric value
                    if let Ok(val) = value_str.parse::<f64>() {
                        return Some(val);
                    }
                }
            }
        }
    }

    // Metric not found - return None, never zero
    None
}

/// Extract a counter that may or may not carry the Prometheus `_total` suffix.
///
/// The two backends this router runs against disagree on the name. Real vLLM
/// v0.26.0, verified on the wire, exports `vllm:num_preemptions_total` and
/// `vllm:prefix_cache_hits_total`. `llm-d-inference-sim` exports the bare
/// `vllm:prefix_cache_hits` and omits preemptions entirely.
///
/// Searching for only one spelling silently yields `None` against the other
/// backend, which is indistinguishable from the metric being genuinely absent.
/// That is the exact confusion this parser exists to prevent, and for
/// preemptions it is the difference between "the gate held" and "we never
/// looked".
///
/// Tries the bare name first, then `<base>_total`. Both lookups go through
/// `metric_value`, which anchors on `{` or whitespace, so the sibling
/// `<base>_created` -- whose value is a unix timestamp, not a count -- can
/// never be matched by accident.
///
/// Returns `None` when neither spelling is present. NEVER `Some(0.0)`.
pub fn counter_value(text: &str, base: &str) -> Option<f64> {
    metric_value(text, base).or_else(|| metric_value(text, &format!("{base}_total")))
}

/// Extract one label's value from an info-style metric where the data
/// lives in labels (e.g., vllm:cache_config_info{block_size="16",num_gpu_blocks="512"} 1).
/// The metric value itself is always 1 and meaningless.
///
/// # Examples
/// ```ignore
/// let text = r#"vllm:cache_config_info{block_size="16",num_gpu_blocks="512"} 1"#;
/// assert_eq!(info_label(text, "vllm:cache_config_info", "block_size"), Some("16".to_string()));
/// ```
pub fn info_label(text: &str, metric: &str, label: &str) -> Option<String> {
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Look for the metric name
        if let Some(after_name) = line.strip_prefix(metric) {
            // Must have labels (start with '{')
            if let Some(brace_start) = after_name.find('{') {
                if let Some(brace_end) = after_name[brace_start..].find('}') {
                    let labels_block = &after_name[brace_start + 1..brace_start + brace_end];
                    
                    // Parse label=value pairs
                    // Format: label="value" or label='value'
                    let label_pattern = format!("{}=", label);
                    if let Some(label_pos) = labels_block.find(&label_pattern) {
                        let after_label = &labels_block[label_pos + label_pattern.len()..];
                        
                        // Extract quoted value
                        if let Some(quote_char) = after_label.chars().next() {
                            if quote_char == '"' || quote_char == '\'' {
                                let value_start = &after_label[1..];
                                if let Some(end_quote) = value_start.find(quote_char) {
                                    return Some(value_start[..end_quote].to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Parse full Prometheus exposition into ReportedLoad.
///
/// `now` MUST be router's own monotonic Instant, never a backend timestamp.
/// `prev` supplies previous counter sample for computing windowed hit rate.
///
/// Returns (ReportedLoad, new_counter_sample).
///
/// # Behavior
/// - Absent metrics return `None` for that field
/// - `kv_cache_usage_perc` is interpreted as fraction 0.0-1.0
/// - `prefix_hit_rate` = Δhits / Δqueries from previous sample
/// - When `prev` is `None` or Δqueries <= 0, `prefix_hit_rate` is `None`
/// - Empty body returns all-`None` ReportedLoad
pub fn parse_reported(
    text: &str,
    now: Instant,
    prev: Option<&CounterSample>,
) -> (ReportedLoad, CounterSample) {
    // Extract current counter values
    let current_hits = counter_value(text, "vllm:prefix_cache_hits");
    let current_queries = counter_value(text, "vllm:prefix_cache_queries");

    // Compute windowed prefix hit rate
    let prefix_hit_rate = if let (Some(prev_sample), Some(cur_hits), Some(cur_queries)) =
        (prev, current_hits, current_queries)
    {
        if let (Some(prev_hits), Some(prev_queries)) =
            (prev_sample.prefix_hits, prev_sample.prefix_queries)
        {
            let delta_hits = cur_hits - prev_hits;
            let delta_queries = cur_queries - prev_queries;

            if delta_queries > 0.0 {
                Some((delta_hits / delta_queries) as f32)
            } else {
                None
            }
        } else {
            // Previous sample had None values
            None
        }
    } else {
        // First sample or counters absent
        None
    };

    // Extract gauge metrics
    let kv_usage_perc = metric_value(text, "vllm:kv_cache_usage_perc").map(|v| v as f32);
    let num_running = metric_value(text, "vllm:num_requests_running").map(|v| v as u32);
    let num_waiting = metric_value(text, "vllm:num_requests_waiting").map(|v| v as u32);
    let preemptions = counter_value(text, "vllm:num_preemptions").map(|v| v as u64);

    let load = ReportedLoad {
        observed_at: now,
        kv_usage_perc,
        num_running,
        num_waiting,
        preemptions,
        prefix_hit_rate,
    };

    let new_sample = CounterSample {
        prefix_hits: current_hits,
        prefix_queries: current_queries,
    };

    (load, new_sample)
}

/// Extract capacity from cache_config_info labels.
///
/// The metric value is always 1 and meaningless - data is in labels.
///
/// # Examples
/// ```ignore
/// let text = r#"vllm:cache_config_info{block_size="16",num_gpu_blocks="512"} 1"#;
/// let cap = parse_capacity(text).unwrap();
/// assert_eq!(cap.block_size, 16);
/// assert_eq!(cap.num_gpu_blocks, 512);
/// ```
pub fn parse_capacity(text: &str) -> Option<ReportedCapacity> {
    let block_size_str = info_label(text, "vllm:cache_config_info", "block_size")?;
    let num_gpu_blocks_str = info_label(text, "vllm:cache_config_info", "num_gpu_blocks")?;

    let block_size = block_size_str.parse::<u32>().ok()?;
    let num_gpu_blocks = num_gpu_blocks_str.parse::<u32>().ok()?;

    Some(ReportedCapacity {
        block_size,
        num_gpu_blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gauge_with_labels() {
        let text = r#"vllm:num_requests_running{model_name="test-model"} 7"#;
        assert_eq!(metric_value(text, "vllm:num_requests_running"), Some(7.0));
    }

    #[test]
    fn parses_gauge_without_labels() {
        let text = "my_metric 42.5\n";
        assert_eq!(metric_value(text, "my_metric"), Some(42.5));
    }

    #[test]
    fn absent_metric_is_none_not_zero() {
        let text = r#"
vllm:num_requests_running{model_name="test-model"} 7
vllm:kv_cache_usage_perc{model_name="test-model"} 0.5
"#;
        // num_preemptions is not present in the text
        assert_eq!(metric_value(text, "vllm:num_preemptions"), None);
    }

    #[test]
    fn skips_help_and_type_lines() {
        let text = r#"
# HELP vllm:num_requests_running Number of requests running
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{model_name="test-model"} 7
"#;
        assert_eq!(metric_value(text, "vllm:num_requests_running"), Some(7.0));
    }

    #[test]
    fn prefix_match_anchored() {
        let text = r#"
vllm:external_prefix_cache_hits{model_name="test"} 100
vllm:prefix_cache_hits{model_name="test"} 50
"#;
        // Should match only the exact metric, not the one with additional prefix
        assert_eq!(metric_value(text, "vllm:prefix_cache_hits"), Some(50.0));
        assert_eq!(
            metric_value(text, "vllm:external_prefix_cache_hits"),
            Some(100.0)
        );
    }

    #[test]
    fn kv_usage_is_fraction_not_percent() {
        let text = r#"vllm:kv_cache_usage_perc{model_name="test-model"} 0.96875"#;
        assert_eq!(
            metric_value(text, "vllm:kv_cache_usage_perc"),
            Some(0.96875)
        );
    }

    #[test]
    fn info_label_extracts_from_labels() {
        let text =
            r#"vllm:cache_config_info{block_size="16",num_gpu_blocks="512"} 1"#;
        assert_eq!(
            info_label(text, "vllm:cache_config_info", "block_size"),
            Some("16".to_string())
        );
        assert_eq!(
            info_label(text, "vllm:cache_config_info", "num_gpu_blocks"),
            Some("512".to_string())
        );
    }

    #[test]
    fn malformed_lines_do_not_panic() {
        let text = r#"
vllm:num_requests_running{model_name="test" 7
garbage
vllm:kv_cache_usage_perc 0.5
incomplete{
"#;
        // Should parse what it can without panicking
        assert_eq!(metric_value(text, "vllm:kv_cache_usage_perc"), Some(0.5));
        // Malformed metric returns None
        assert_eq!(metric_value(text, "vllm:num_requests_running"), None);
    }

    #[test]
    fn empty_body_is_all_none() {
        let text = "";
        let now = Instant::now();
        let (load, _) = parse_reported(text, now, None);
        
        assert!(load.kv_usage_perc.is_none());
        assert!(load.num_running.is_none());
        assert!(load.num_waiting.is_none());
        assert!(load.preemptions.is_none());
        assert!(load.prefix_hit_rate.is_none());
    }

    #[test]
    fn prefix_hit_rate_is_windowed_delta() {
        let text1 = r#"
vllm:prefix_cache_hits{model_name="test"} 100
vllm:prefix_cache_queries{model_name="test"} 200
"#;
        let text2 = r#"
vllm:prefix_cache_hits{model_name="test"} 150
vllm:prefix_cache_queries{model_name="test"} 300
"#;

        let now = Instant::now();
        let (load1, sample1) = parse_reported(text1, now, None);
        assert!(load1.prefix_hit_rate.is_none()); // First sample has no rate

        let (load2, _) = parse_reported(text2, now, Some(&sample1));
        // Δhits = 150 - 100 = 50, Δqueries = 300 - 200 = 100
        // Rate = 50/100 = 0.5
        assert_eq!(load2.prefix_hit_rate, Some(0.5));
    }

    #[test]
    fn first_sample_has_no_hit_rate() {
        let text = r#"
vllm:prefix_cache_hits{model_name="test"} 100
vllm:prefix_cache_queries{model_name="test"} 200
"#;
        let now = Instant::now();
        let (load, _) = parse_reported(text, now, None);
        
        // First sample with prev == None should have no hit rate
        assert!(load.prefix_hit_rate.is_none());
    }

    #[test]
    fn observed_at_is_passed_instant() {
        let text = r#"vllm:kv_cache_usage_perc{model_name="test"} 0.5"#;
        let now = Instant::now();
        let (load, _) = parse_reported(text, now, None);
        
        // observed_at should be the passed instant
        assert_eq!(load.observed_at, now);
    }

    #[test]
    fn parse_capacity_extracts_correctly() {
        let text =
            r#"vllm:cache_config_info{block_size="16",num_gpu_blocks="512"} 1"#;
        let cap = parse_capacity(text).unwrap();
        assert_eq!(cap.block_size, 16);
        assert_eq!(cap.num_gpu_blocks, 512);
    }

    #[test]
    fn parse_capacity_returns_none_when_absent() {
        let text = "vllm:num_requests_running{model_name=\"test\"} 7";
        assert!(parse_capacity(text).is_none());
    }
}
