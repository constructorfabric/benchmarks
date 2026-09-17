//! Tests for the outbound scheme and address screens.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use super::{DialRefusal, SsrfPolicy, plaintext_allowed, refusal_failure, screen_address};
use crate::domain::model::EndpointScheme;

fn enabled() -> SsrfPolicy {
    SsrfPolicy { enabled: true }
}

#[test]
fn plaintext_gate_depends_on_the_configuration() {
    assert!(plaintext_allowed(EndpointScheme::Https, false));
    assert!(!plaintext_allowed(EndpointScheme::Http, false));
    assert!(plaintext_allowed(EndpointScheme::Http, true));
    assert!(plaintext_allowed(EndpointScheme::Grpc, false));
    assert!(plaintext_allowed(EndpointScheme::Https, false));
}

#[test]
fn disabled_policy_screens_nothing() {
    assert!(screen_address(IpAddr::V4(Ipv4Addr::LOCALHOST), SsrfPolicy::disabled()).is_ok());
    assert!(
        screen_address(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            SsrfPolicy::disabled()
        )
        .is_ok()
    );
}

#[test]
fn loopback_is_screened() {
    let refusal = screen_address(IpAddr::V4(Ipv4Addr::LOCALHOST), enabled()).unwrap_err();
    assert!(matches!(
        refusal,
        DialRefusal::Screened {
            reason: "loopback",
            ..
        }
    ));
}

#[test]
fn private_and_link_local_space_is_screened() {
    for literal in [
        "10.1.2.3",
        "172.16.0.9",
        "192.168.1.1",
        "169.254.1.1",
        "127.0.0.1",
    ] {
        let address = IpAddr::from_str(literal).expect("ipv4");
        let error = screen_address(address, enabled()).expect_err(literal);
        let DialRefusal::Screened { address: shown, .. } = error else {
            panic!("{literal} should be screened");
        };
        assert_eq!(shown, literal);
    }
}

#[test]
fn reserved_space_is_screened() {
    for (literal, reason) in [
        ("100.64.0.1", "shared-address-space"),
        ("198.18.0.1", "benchmarking"),
        ("192.0.2.1", "documentation"),
        ("203.0.113.9", "documentation"),
        ("0.0.0.1", "this-network"),
    ] {
        let address = IpAddr::from_str(literal).expect("ipv4");
        match screen_address(address, enabled()) {
            Err(DialRefusal::Screened { reason: shown, .. }) => assert_eq!(shown, reason),
            other => panic!("{literal} should be screened as {reason}, got {other:?}"),
        }
    }
}

#[test]
fn public_space_is_dialable() {
    for literal in ["93.184.216.34", "1.1.1.1", "8.8.4.4"] {
        let address = IpAddr::from_str(literal).expect("ipv4");
        assert!(screen_address(address, enabled()).is_ok(), "{literal}");
    }
}

#[test]
fn ipv6_loopback_and_local_are_screened() {
    let loopback = IpAddr::V6(Ipv6Addr::LOCALHOST);
    assert!(matches!(
        screen_address(loopback, enabled()),
        Err(DialRefusal::Screened {
            reason: "loopback",
            ..
        })
    ));

    let unique_local = IpAddr::from_str("fd00::1").expect("ipv6");
    assert!(matches!(
        screen_address(unique_local, enabled()),
        Err(DialRefusal::Screened {
            reason: "unique-local",
            ..
        })
    ));

    let link_local = IpAddr::from_str("fe80::1").expect("ipv6");
    assert!(screen_address(link_local, enabled()).is_err());
}

#[test]
fn ipv4_mapped_ipv6_is_screened_on_its_v4_half() {
    let mapped = IpAddr::from_str("::ffff:127.0.0.1").expect("mapped");
    assert!(screen_address(mapped, enabled()).is_err());
}

#[test]
fn public_ipv6_is_dialable() {
    let address = IpAddr::from_str("2606:4700:4700::1111").expect("ipv6");
    assert!(screen_address(address, enabled()).is_ok());
}

#[test]
fn screened_refusal_renders_a_403_failure() {
    let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let refusal = screen_address(address, enabled()).unwrap_err();
    let failure = refusal_failure("internal.example", &refusal);
    assert_eq!(failure.status, 403);
    assert_eq!(
        failure.source,
        crate::infra::proxy::failure::ErrorSource::Gateway
    );
}

#[test]
fn scheme_refusal_renders_a_400_failure() {
    let refusal = DialRefusal::SchemeDisallowed {
        host: "internal.example".to_owned(),
    };
    let failure = refusal_failure("internal.example", &refusal);
    assert_eq!(failure.status, 400);
}
