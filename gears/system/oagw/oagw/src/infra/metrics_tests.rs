//! Metric helper tests.

use crate::infra::metrics::{DURATION_BUCKETS, normalize_method, state_label};
use crate::infra::ratelimit::CircuitState;

#[test]
fn duration_buckets_match_design() {
    assert_eq!(DURATION_BUCKETS[0], 0.001);
    assert_eq!(DURATION_BUCKETS[DURATION_BUCKETS.len() - 1], 10.0);
    assert_eq!(DURATION_BUCKETS.len(), 12);
}

#[test]
fn methods_are_normalized_to_semconv() {
    assert_eq!(normalize_method("GET"), "GET");
    assert_eq!(normalize_method("PATCH"), "PATCH");
    assert_eq!(normalize_method("PURGE"), "_OTHER");
}

#[test]
fn state_labels_are_snake_case() {
    assert_eq!(state_label(CircuitState::Closed), "closed");
    assert_eq!(state_label(CircuitState::HalfOpen), "half_open");
    assert_eq!(state_label(CircuitState::Open), "open");
}
