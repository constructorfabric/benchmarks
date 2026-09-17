//! Egress-policy tests.

use super::{forbidden, gate};
use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::model::{Endpoint, Scheme};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// An endpoint to `host` with the given scheme.
fn endpoint(scheme: Scheme, host: &str) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port: scheme.standard_port(),
    }
}

/// A config with plaintext allowed and the SSRF policy in `enabled` state.
fn config(allow_http: bool, ssrf: bool) -> OagwConfig {
    OagwConfig {
        allow_http_upstream: allow_http,
        ssrf_policy: SsrfPolicy { enabled: ssrf },
        ..OagwConfig::default()
    }
}

#[tokio::test]
async fn plaintext_is_refused_when_the_gate_is_closed() {
    let error = gate(
        &config(false, false),
        &endpoint(Scheme::Http, "example.com"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind.http_status(), 503);
    assert_eq!(
        error.kind.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );
}

#[tokio::test]
async fn plaintext_is_dialled_when_the_gate_is_open() {
    assert!(
        gate(&config(true, false), &endpoint(Scheme::Http, "example.com"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_scheme_gate_never_touches_a_secure_upstream() {
    assert!(
        gate(
            &config(false, false),
            &endpoint(Scheme::Https, "example.com")
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn a_loopback_literal_is_refused_while_the_policy_is_on() {
    let error = gate(&config(true, true), &endpoint(Scheme::Http, "127.0.0.1"))
        .await
        .unwrap_err();
    assert_eq!(error.kind, crate::domain::error::ErrorKind::LinkUnavailable);
}

#[tokio::test]
async fn a_private_literal_is_refused_while_the_policy_is_on() {
    assert!(
        gate(&config(true, true), &endpoint(Scheme::Https, "10.1.2.3"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_public_literal_passes_the_policy() {
    assert!(
        gate(&config(true, true), &endpoint(Scheme::Https, "203.0.113.7"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_policy_may_be_switched_off_for_a_private_fabric() {
    assert!(
        gate(&config(true, false), &endpoint(Scheme::Http, "127.0.0.1"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_name_pointing_at_loopback_is_refused() {
    // `localhost` resolves to 127.0.0.1 in every test environment.
    assert!(
        gate(&config(true, true), &endpoint(Scheme::Http, "localhost"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_unresolvable_name_is_refused_while_the_policy_is_on() {
    let error = gate(
        &config(true, true),
        &endpoint(Scheme::Https, "no.such.host.invalid"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, crate::domain::error::ErrorKind::LinkUnavailable);
}

#[test]
fn ipv4_ranges_are_classified() {
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        "192.168.1.1",
        "169.254.1.1",
        "0.0.0.0",
    ] {
        let address: IpAddr = address
            .parse()
            .unwrap_or_else(|error| panic!("{address}: {error}"));
        assert!(forbidden(address), "{address} must be forbidden");
    }
    assert!(!forbidden(IpAddr::from(Ipv4Addr::new(203, 0, 113, 7))));
}

#[test]
fn ipv6_and_mapped_ranges_are_classified() {
    for address in ["::1", "::", "fe80::1", "fc00::1", "::ffff:127.0.0.1"] {
        let address: IpAddr = address
            .parse()
            .unwrap_or_else(|error| panic!("{address}: {error}"));
        assert!(forbidden(address), "{address} must be forbidden");
    }
    assert!(!forbidden(IpAddr::from(Ipv6Addr::new(
        0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111
    ))));
}
