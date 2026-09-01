//! `ProxyPolicy` tests (`DESIGN` §2.2, timeout and plaintext-upstream knobs).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::time::Duration;

use crate::domain::error::DomainError;
use crate::infra::proxy::policy::ProxyPolicy;

#[test]
fn the_deployment_budget_becomes_the_upstream_deadline() {
    let policy = ProxyPolicy::new(2, true);
    assert_eq!(policy.proxy_timeout, Duration::from_secs(2));
    assert!(policy.allow_http_upstream);
}

#[test]
fn a_zero_budget_is_clamped_to_one_second() {
    let policy = ProxyPolicy::new(0, false);
    assert_eq!(policy.proxy_timeout, Duration::from_secs(1));
    assert!(!policy.allow_http_upstream);
}

#[test]
fn an_expired_budget_is_a_terminal_504() {
    let policy = ProxyPolicy::new(15, false);
    let DomainError::RequestTimeout { limit_secs } = policy.request_timeout() else {
        panic!("expected RequestTimeout");
    };
    assert_eq!(limit_secs, 15);
}
