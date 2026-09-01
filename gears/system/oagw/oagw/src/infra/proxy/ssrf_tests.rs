//! SSRF guard tests (`DESIGN` §4.4, security considerations).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use crate::domain::error::DomainError;
use crate::infra::proxy::ssrf::SsrfGuard;

#[test]
fn a_disabled_guard_is_a_no_op() {
    let guard = SsrfGuard::disabled();
    assert!(!guard.is_enabled());
    assert!(guard.check("127.0.0.1", false).is_ok());
    assert!(guard.check("localhost", false).is_ok());
    assert!(guard.check("169.254.169.254", false).is_ok());
}

#[test]
fn a_plaintext_target_is_refused_when_the_guard_watches_outbound_calls() {
    let guard = SsrfGuard::new(true);
    assert!(guard.is_enabled());
    let error = guard.check("api.example.com", false).unwrap_err();
    let DomainError::LinkUnavailable {
        detail,
        retry_after,
    } = error
    else {
        panic!("expected LinkUnavailable");
    };
    assert!(
        detail.contains("plaintext"),
        "the refusal names the transport"
    );
    assert!(retry_after.is_none());
}

#[test]
fn a_tls_target_on_a_public_name_is_admitted() {
    let guard = SsrfGuard::new(true);
    assert!(guard.check("api.example.com", true).is_ok());
    assert!(guard.check("  api.example.com  ", true).is_ok());
    assert!(guard.check("8.8.8.8", true).is_ok());
    assert!(guard.check("[2001:db8::1]", true).is_ok());
}

#[test]
fn a_loopback_target_is_refused() {
    let guard = SsrfGuard::new(true);
    for host in ["localhost", "127.0.0.1", "[::1]", "my-service.localhost"] {
        let error = guard.check(host, true).unwrap_err();
        let DomainError::AccessDenied { detail } = error else {
            panic!("expected AccessDenied for {host}");
        };
        assert!(
            detail.contains("protected address range"),
            "the refusal names the range: {detail}"
        );
    }
}

#[test]
fn a_private_or_link_local_target_is_refused() {
    let guard = SsrfGuard::new(true);
    for host in [
        "10.0.0.5",
        "192.168.1.10",
        "172.16.0.9",
        "169.254.169.254",
        "255.255.255.255",
        "0.0.0.0",
        "10.0.0.5:8080",
        "[fc00::1]",
        "[fe80::1]",
        "printer.local",
    ] {
        assert!(guard.check(host, true).is_err(), "{host} must be refused");
    }
}

#[test]
fn a_global_address_that_merely_looks_private_is_admitted() {
    let guard = SsrfGuard::new(true);
    assert!(guard.check("example.com", true).is_ok());
    assert!(guard.check("info.example.localhosts", true).is_ok());
}
