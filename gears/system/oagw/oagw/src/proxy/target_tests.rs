//! Tests for the SSRF policy the relay consults before dialling (FR-034).

use crate::config::SsrfPolicy;
use crate::proxy::target::check_ssrf;

/// The policy ships enabled with no lists, so the range checks are the whole of it.
#[test]
fn an_unconfigured_policy_refuses_the_local_ranges() {
    let policy = SsrfPolicy::default();
    assert!(policy.enabled, "the policy is on unless the configuration turns it off");

    assert!(check_ssrf(&policy, "upstream.example.com").is_ok());
    for refused in [
        "127.0.0.1",
        "127.8.9.10",
        "10.1.2.3",
        "192.168.0.5",
        "172.16.4.5",
        "169.254.169.254",
        "0.0.0.0",
        "::1",
        "fc00::1",
        "fe80::1",
    ] {
        assert!(
            check_ssrf(&policy, refused).is_err(),
            "{refused} is in a range the gear will not contact"
        );
    }
}

/// A public literal is dialled.
#[test]
fn a_public_address_is_dialled() {
    assert!(check_ssrf(&SsrfPolicy::default(), "93.184.216.34").is_ok());
    assert!(check_ssrf(&SsrfPolicy::default(), "2606:2800:220:1:248:1893:25c8:1946").is_ok());
}

/// The deny list is honoured as configuration, and it wins over the allow list.
#[test]
fn a_denied_host_is_refused_even_when_it_is_also_allowed() {
    let policy = SsrfPolicy {
        deny_hosts: vec!["payments.example.com".to_owned()],
        allow_hosts: vec!["payments.example.com".to_owned()],
        ..SsrfPolicy::default()
    };
    assert!(check_ssrf(&policy, "payments.example.com").is_err());
    assert!(
        check_ssrf(&policy, "PAYMENTS.Example.COM.").is_err(),
        "the list is matched on the normalized host"
    );
}

/// A deny entry covers the names beneath it, not just itself.
#[test]
fn a_deny_entry_covers_the_subdomains_beneath_it() {
    let policy = SsrfPolicy {
        deny_hosts: vec!["internal.example.com".to_owned()],
        ..SsrfPolicy::default()
    };
    assert!(check_ssrf(&policy, "api.internal.example.com").is_err());
    assert!(check_ssrf(&policy, "example.com").is_ok());
}

/// An allow entry exempts a host from the range checks, which is how a deployment dials
/// a loopback neighbour on purpose.
#[test]
fn an_allowed_host_is_exempt_from_the_range_checks() {
    let policy = SsrfPolicy {
        allow_hosts: vec!["127.0.0.1".to_owned()],
        ..SsrfPolicy::default()
    };
    assert!(check_ssrf(&policy, "127.0.0.1").is_ok());
    assert!(check_ssrf(&policy, "10.0.0.1").is_err());
}

/// Disabling the policy turns the whole check off: honouring the flag is the point.
#[test]
fn a_disabled_policy_refuses_nothing() {
    let policy = SsrfPolicy {
        enabled: false,
        deny_hosts: vec!["payments.example.com".to_owned()],
        ..SsrfPolicy::default()
    };
    assert!(check_ssrf(&policy, "127.0.0.1").is_ok());
    assert!(check_ssrf(&policy, "payments.example.com").is_ok());
}

/// A deny entry is matched by exact normalized host; a bare dot or empty entry matches
/// nothing.
#[test]
fn an_empty_deny_entry_matches_nothing() {
    let policy = SsrfPolicy {
        deny_hosts: vec![String::new()],
        ..SsrfPolicy::default()
    };
    assert!(check_ssrf(&policy, "example.com").is_ok());
}
