// Updated: 2026-09-01 by Constructor Tech
//! SSRF screening for upstream endpoints (PRD `cpt-cf-oagw-fr-ssrf-protection`).
//!
//! Two layers, deliberately kept apart:
//!
//! * [`is_denied_host`] is the *schema* screen applied at create time. It runs
//!   on the literal the operator typed and never resolves anything, so it is
//!   cheap, deterministic, and cannot be defeated by DNS. It rejects obvious
//!   loopback / link-local / private-network literals and the cloud metadata
//!   hostnames.
//! * [`SsrfGuard`] is the *dial* screen. It runs on the addresses actually
//!   returned by resolution, immediately before a socket is opened, so a
//!   hostname that resolves into a private range is caught even though the
//!   literal looked innocent.
//!
//! Screening is disabled entirely when `ssrf_policy.enabled` is `false`
//! (the development posture in `config/e2e-local.yaml`).

use std::net::IpAddr;

use crate::config::SsrfPolicy;

/// Hostnames that resolve to infrastructure OAGW must never be asked to dial.
const DENIED_HOSTNAMES: [&str; 4] = [
    "localhost",
    "metadata.google.internal",
    "instance-data",
    "metadata",
];

/// Whether `host` is a literal or name that OAGW refuses outright.
#[must_use]
pub fn is_denied_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    let lower = h.to_ascii_lowercase();
    if DENIED_HOSTNAMES.contains(&lower.as_str()) {
        return true;
    }
    match lower.parse::<IpAddr>() {
        Ok(ip) => is_private_address(&ip),
        Err(_) => false,
    }
}

/// Whether `ip` falls in a range that is never dialled.
#[must_use]
pub fn is_private_address(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.octets()[0] == 0 // "this" network
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || is_unicast_link_local_v6(v6) || is_ula_v6(v6)
        }
    }
}

fn is_unicast_link_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

fn is_ula_v6(v6: &std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// Why an endpoint was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SsrfRejection {
    #[error("host '{host}' resolves into a blocked network range")]
    BlockedRange { host: String },
    #[error("host '{host}' could not be resolved")]
    Unresolvable { host: String },
}

/// Decide whether `resolved` may be dialled.
///
/// A no-op when the policy is disabled, so the development configuration keeps
/// working.
pub fn check(host: &str, resolved: &[IpAddr], policy: &SsrfPolicy) -> Result<(), SsrfRejection> {
    if !policy.enabled {
        return Ok(());
    }
    if resolved.is_empty() {
        return if policy.deny_unresolvable {
            Err(SsrfRejection::Unresolvable {
                host: host.to_owned(),
            })
        } else {
            Ok(())
        };
    }
    let blocked = resolved.iter().any(|ip| {
        is_private_address(ip)
            || policy
                .denied_cidrs
                .iter()
                .any(|cidr| cidr_contains(cidr, ip))
    });
    if blocked {
        Err(SsrfRejection::BlockedRange {
            host: host.to_owned(),
        })
    } else {
        Ok(())
    }
}

/// Minimal CIDR match supporting `a.b.c.d/nn` and `x::/nn`.
fn cidr_contains(cidr: &str, ip: &IpAddr) -> bool {
    let Some((net, mask)) = cidr.split_once('/') else {
        return cidr
            .trim()
            .parse::<IpAddr>()
            .map(|n| &n == ip)
            .unwrap_or(false);
    };
    let Ok(mask) = mask.trim().parse::<u32>() else {
        return false;
    };
    let Ok(net_ip) = net.trim().parse::<IpAddr>() else {
        return false;
    };
    match (net_ip, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) => {
            if mask > 32 {
                return false;
            }
            let n = u32::from(n);
            let i = u32::from(*i);
            if mask == 0 {
                true
            } else {
                let m = u32::MAX << (32 - mask);
                (n & m) == (i & m)
            }
        }
        (IpAddr::V6(n), IpAddr::V6(i)) => {
            if mask > 128 {
                return false;
            }
            let n = u128::from(n);
            let i = u128::from(*i);
            if mask == 0 {
                true
            } else {
                let m = u128::MAX << (128 - mask);
                (n & m) == (i & m)
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn literal_private_addresses_are_denied() {
        for host in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.5",
            "172.16.0.1",
            "169.254.169.254",
            "::1",
            "localhost",
        ] {
            assert!(is_denied_host(host), "{host}");
        }
    }

    #[test]
    fn public_literals_and_names_are_allowed() {
        for host in [
            "api.openai.com",
            "93.184.216.34",
            "2606:2800:220:1:248:1893:25c8:1946",
        ] {
            assert!(!is_denied_host(host), "{host}");
        }
    }

    #[test]
    fn guard_blocks_resolved_private_address() {
        let policy = SsrfPolicy::default();
        let err = check(
            "internal.corp",
            &[IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9))],
            &policy,
        )
        .unwrap_err();
        assert!(matches!(err, SsrfRejection::BlockedRange { .. }));
    }

    #[test]
    fn guard_allows_public_address() {
        let policy = SsrfPolicy::default();
        assert!(
            check(
                "example.com",
                &[IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
                &policy
            )
            .is_ok()
        );
    }

    #[test]
    fn guard_can_be_disabled() {
        let policy = SsrfPolicy {
            enabled: false,
            ..SsrfPolicy::default()
        };
        assert!(check("127.0.0.1", &[IpAddr::V4(Ipv4Addr::LOCALHOST)], &policy).is_ok());
    }

    #[test]
    fn guard_reports_unresolvable() {
        let policy = SsrfPolicy::default();
        let err = check("missing.invalid", &[], &policy).unwrap_err();
        assert!(matches!(err, SsrfRejection::Unresolvable { .. }));
    }

    #[test]
    fn cidr_matching() {
        assert!(cidr_contains(
            "10.0.0.0/8",
            &IpAddr::V4(Ipv4Addr::new(10, 255, 0, 1))
        ));
        assert!(!cidr_contains(
            "10.0.0.0/8",
            &IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1))
        ));
        assert!(cidr_contains(
            "0.0.0.0/0",
            &IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))
        ));
        assert!(cidr_contains(
            "::1/128",
            &IpAddr::V6("::1".parse().unwrap())
        ));
        assert!(!cidr_contains(
            "fe80::/10",
            &IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))
        ));
    }
}
