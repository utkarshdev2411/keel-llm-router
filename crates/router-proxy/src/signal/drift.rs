//! Projection drift detection: compares router's KV projection against backend reality.
//!
//! This module computes the ratio of router-projected KV usage to backend-reported KV usage.
//! The drift metric validates that the router's internal accounting matches what the backend
//! actually observes.
//!
//! **Critical invariant:** This is a cross-check mechanism only. Drift values MUST NOT
//! influence routing decisions.

use router_core::backend::{Backend, ReportedLoad};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Compute projection drift: (router projected / capacity) ÷ (backend reported usage).
///
/// Returns `None` when there is nothing meaningful to compare:
/// - Reported metrics are stale (age > max_age)
/// - kv_usage_perc is absent from backend
/// - Either side < 5% (idle, avoid dividing near-zero by near-zero)
///
/// Returns `Some(ratio)` where:
/// - ratio ≈ 1.0 means router and backend agree
/// - ratio > 1.0 means router projects more than backend reports (possible double-counting)
/// - ratio < 1.0 means router projects less than backend reports (unexpected)
///
/// # Arguments
///
/// * `backend` - The backend to check
/// * `reported` - The scraped metrics from the backend
/// * `now` - Current time (router monotonic)
/// * `max_age` - Maximum acceptable age for reported metrics
///
/// # Example
///
/// ```ignore
/// let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
/// match drift {
///     Some(ratio) if ratio > 2.0 => warn!("Significant projection drift: {:.2}x", ratio),
///     Some(ratio) => debug!("Drift: {:.2}x", ratio),
///     None => trace!("No meaningful drift comparison available"),
/// }
/// ```
pub fn projection_drift(
    backend: &Backend,
    reported: &ReportedLoad,
    now: Instant,
    max_age: Duration,
) -> Option<f64> {
    // Guard 1: If reported metrics are stale, we can't trust them for comparison
    if reported.is_stale(now, max_age) {
        return None;
    }

    // Guard 2: If kv_usage_perc is absent, we have nothing to compare against
    let r = reported.kv_usage_perc?;

    // Guard 3: If backend reports < 5% usage, denominator is too small (idle backend)
    if r < 0.05 {
        return None;
    }

    // Compute router's projected KV usage as a fraction
    let kv_projected = backend.live.kv_projected_tokens.load(Ordering::Relaxed);
    let kv_capacity = backend.caps.kv_capacity_tokens as f64;
    
    // kv_projected can be negative during transient accounting (rare), treat as 0
    let p = kv_projected.max(0) as f64 / kv_capacity;

    // Guard 4: If router thinks backend is < 5% full, numerator is too small
    if p < 0.05 {
        return None;
    }

    // Both sides have meaningful values, compute the ratio
    Some(p / r as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_core::backend::{Backend, BackendId, CapsEstimate, HealthState, LiveCounters, ReportedLoad};
    use std::sync::atomic::AtomicI64;
    use std::sync::Arc;
    use arc_swap::ArcSwapOption;

    fn make_backend(kv_capacity: u32, kv_projected: i64) -> Backend {
        Backend {
            id: BackendId(1),
            key: Arc::from("test-backend"),
            uri: Arc::from("http://localhost:8000"),
            model: Arc::from("test-model"),
            weight: 1.0,
            caps: CapsEstimate {
                kv_capacity_tokens: kv_capacity,
                max_num_seqs: 64,
            },
            live: LiveCounters {
                kv_projected_tokens: AtomicI64::new(kv_projected),
                ..Default::default()
            },
            reported: ArcSwapOption::empty(),
            health: HealthState::default(),
        }
    }

    fn make_reported(kv_usage_perc: Option<f32>, age_ms: u64) -> ReportedLoad {
        let observed_at = Instant::now() - Duration::from_millis(age_ms);
        ReportedLoad {
            observed_at,
            kv_usage_perc,
            num_running: None,
            num_waiting: None,
            preemptions: None,
            prefix_hit_rate: None,
        }
    }

    #[test]
    fn idle_backend_yields_none() {
        let backend = make_backend(1000, 10); // 1% projected
        let reported = make_reported(Some(0.01), 100); // 1% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert_eq!(drift, None, "Both sides < 5%, should return None");
    }

    #[test]
    fn idle_backend_reported_only() {
        let backend = make_backend(1000, 200); // 20% projected
        let reported = make_reported(Some(0.01), 100); // 1% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert_eq!(drift, None, "Backend reports < 5%, should return None");
    }

    #[test]
    fn idle_backend_projected_only() {
        let backend = make_backend(1000, 10); // 1% projected
        let reported = make_reported(Some(0.20), 100); // 20% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert_eq!(drift, None, "Router projects < 5%, should return None");
    }

    #[test]
    fn matched_projection_near_one() {
        let backend = make_backend(1000, 500); // 50% projected
        let reported = make_reported(Some(0.50), 100); // 50% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert!(drift.is_some());
        let ratio = drift.unwrap();
        assert!((ratio - 1.0).abs() < 0.01, "Equal fractions should yield ~1.0, got {:.3}", ratio);
    }

    #[test]
    fn router_double_counting_reads_two() {
        let backend = make_backend(1000, 800); // 80% projected
        let reported = make_reported(Some(0.40), 100); // 40% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert!(drift.is_some());
        let ratio = drift.unwrap();
        assert!((ratio - 2.0).abs() < 0.01, "2x projection should yield ~2.0, got {:.3}", ratio);
    }

    #[test]
    fn stale_reading_yields_none() {
        let backend = make_backend(1000, 500); // 50% projected
        let reported = make_reported(Some(0.50), 6000); // 6 seconds old
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert_eq!(drift, None, "Stale reading (6s > 5s max) should return None");
    }

    #[test]
    fn absent_kv_metric_yields_none() {
        let backend = make_backend(1000, 500); // 50% projected
        let reported = make_reported(None, 100); // kv_usage_perc absent, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert_eq!(drift, None, "Absent kv_usage_perc should return None");
    }

    #[test]
    fn negative_projected_treated_as_zero() {
        let backend = make_backend(1000, -100); // Negative (transient accounting)
        let reported = make_reported(Some(0.50), 100); // 50% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert_eq!(drift, None, "Negative projected (clamped to 0) yields < 5%, should return None");
    }

    #[test]
    fn exact_five_percent_threshold() {
        // Test that exactly 5% is NOT excluded (< 0.05 means 5% passes the guard)
        let backend = make_backend(1000, 50); // Exactly 5% projected
        let reported = make_reported(Some(0.05), 100); // Exactly 5% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert!(drift.is_some(), "Exactly 5% should compute drift (not < 0.05)");
        let ratio = drift.unwrap();
        assert!((ratio - 1.0).abs() < 0.01, "Equal fractions should yield ~1.0, got {:.3}", ratio);
    }

    #[test]
    fn just_above_five_percent_threshold() {
        // Test that just above 5% works
        let backend = make_backend(1000, 51); // 5.1% projected
        let reported = make_reported(Some(0.051), 100); // 5.1% reported, fresh
        
        let drift = projection_drift(&backend, &reported, Instant::now(), Duration::from_secs(5));
        assert!(drift.is_some(), "Just above 5% should compute drift");
        let ratio = drift.unwrap();
        assert!((ratio - 1.0).abs() < 0.01, "Equal fractions should yield ~1.0, got {:.3}", ratio);
    }
}
