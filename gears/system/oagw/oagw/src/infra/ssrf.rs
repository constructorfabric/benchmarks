//! SSRF guard (DESIGN §3.2 "Security Considerations", §4.4).
//!
//! Static host checks applied on the write path and before dialling. DNS
//! resolution and IP pinning remain a separate concern (DESIGN §4.5).

use std::net::IpAddr;
use std::str::FromStr;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, DomainResult};

/// IPv4 segment classes that are never routable from the data plane.
const BLOCKED_V4_SEGMENTS: [(&str, u8); 6] = [
    ("127.0.0.0", 8),
    ("10.0.0.0", 8),
    ("172.16.0.0", 12),
    ("192.168.0.0", 16),
    ("169.254.0.0", 16),
    ("0.0.0.0", 8),
];

/// IPv6 segment classes that are never routable from the data plane.
const BLOCKED_V6_SEGMENTS: [(&str, u8); 4] =
    [("::1", 128), ("fc00::", 7), ("fe80::", 10), ("::", 128)];

/// Parses a CIDR entry, rejecting masks that do not fit the address family.
fn parse_masked(prefix: &str, mask: u8) -> Option<(IpAddr, u8)> {
    let addr = IpAddr::from_str(prefix).ok()?;
    let width = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    (mask <= width).then_some((addr, mask))
}

/// `true` when `candidate` falls inside the CIDR `segment`.
fn in_segment(candidate: &IpAddr, segment: &(IpAddr, u8)) -> bool {
    match (candidate, segment.0) {
        (IpAddr::V4(candidate), IpAddr::V4(base)) => {
            let mask = prefix_mask_v4(segment.1);
            u32::from(*candidate) & mask == u32::from(base) & mask
        }
        (IpAddr::V6(candidate), IpAddr::V6(base)) => {
            let mask = prefix_mask_v6(segment.1);
            u128::from(*candidate) & mask == u128::from(base) & mask
        }
        // A v4 literal is also covered by the v6 policy through its mapped
        // form, which `canonical` already normalised away.
        _ => false,
    }
}

const fn prefix_mask_v4(mask: u8) -> u32 {
    if mask == 0 {
        0
    } else {
        u32::MAX << (32 - mask)
    }
}

const fn prefix_mask_v6(mask: u8) -> u128 {
    if mask == 0 {
        0
    } else {
        u128::MAX << (128 - mask)
    }
}

/// Normalises a host so segment comparisons cannot be bypassed by an exotic
/// spelling of the same address.
///
/// IPv4-mapped and IPv4-compatible IPv6 literals are folded back to their IPv4
/// form, so `::ffff:127.0.0.1` is judged as `127.0.0.1`.
#[must_use]
pub fn canonical_host(host: &str) -> String {
    let host = host.trim();
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    let Ok(address) = IpAddr::from_str(bare) else {
        return bare.to_ascii_lowercase();
    };
    match address {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => mapped.to_string(),
            None => {
                // `::a.b.c.d` (IPv4-compatible) points at the IPv4 address its
                // low 32 bits encode, so it must be judged as that address and
                // not as an exotic IPv6 spelling of something else.
                let segments = v6.segments();
                if segments[..6] == [0, 0, 0, 0, 0, 0] && segments[6] != 0 {
                    std::net::Ipv4Addr::new(
                        (segments[6] >> 8) as u8,
                        (segments[6] & 0xff) as u8,
                        (segments[7] >> 8) as u8,
                        (segments[7] & 0xff) as u8,
                    )
                    .to_string()
                } else {
                    v6.to_string()
                }
            }
        },
    }
}

fn matches_segment_list(candidate: &IpAddr, segments: &[String]) -> bool {
    segments.iter().any(|segment| {
        let (prefix, mask_text) = match segment.split_once('/') {
            Some((prefix, mask)) => (prefix, mask),
            None => return false,
        };
        let Ok(mask) = mask_text.parse::<u8>() else {
            return false;
        };
        parse_masked(prefix, mask).is_some_and(|parsed| in_segment(candidate, &parsed))
    })
}

/// Label-boundary hostname match: `evil.example` must not match the allowlist
/// entry `example`, and `api.example.com` must match `example.com`.
fn host_matches_domain(host: &str, domain: &str) -> bool {
    let host = host.strip_prefix('.').unwrap_or(host);
    let domain = domain.strip_prefix('.').unwrap_or(domain);
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// Checks a host (IP literal or hostname) against the SSRF policy.
///
/// # Errors
///
/// Returns a validation error when the policy is enabled and the host resolves
/// to a blocked segment without an allowlist entry.
pub fn check_host(host: &str, config: &OagwConfig) -> DomainResult<()> {
    if !config.ssrf_policy.enabled {
        return Ok(());
    }
    let host = canonical_host(host);
    if config
        .ssrf_policy
        .allowed_segments
        .iter()
        .any(|allowed| host_matches_domain(&host, &canonical_host(allowed)))
    {
        return Ok(());
    }
    if config
        .ssrf_policy
        .blocked_segments
        .iter()
        .any(|blocked| host_matches_domain(&host, &canonical_host(blocked)))
    {
        return Err(DomainError::validation(format!(
            "upstream host {host:?} is blocked by the SSRF policy"
        )));
    }
    if let Ok(address) = IpAddr::from_str(&host) {
        let blocked = BLOCKED_V4_SEGMENTS
            .iter()
            .chain(BLOCKED_V6_SEGMENTS.iter())
            .filter_map(|(prefix, mask)| parse_masked(prefix, *mask))
            .any(|segment| in_segment(&address, &segment));
        let allowlisted = matches_segment_list(&address, &config.ssrf_policy.allowed_segments);
        let blocked_by_policy =
            matches_segment_list(&address, &config.ssrf_policy.blocked_segments);
        if (blocked || blocked_by_policy) && !allowlisted {
            return Err(DomainError::validation(format!(
                "upstream host {host:?} points at a non-routable segment blocked by the SSRF policy"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> OagwConfig {
        let mut config = OagwConfig::default();
        config.ssrf_policy.enabled = true;
        config
    }

    #[test]
    fn disabled_policy_allows_everything() {
        let config = OagwConfig::default();
        check_host("127.0.0.1", &config).expect("allowed");
    }

    #[test]
    fn loopback_and_private_segments_are_blocked() {
        let config = enabled();
        assert!(check_host("127.0.0.1", &config).is_err());
        assert!(check_host("10.0.1.1", &config).is_err());
        assert!(check_host("192.168.1.5", &config).is_err());
        assert!(check_host("::1", &config).is_err());
        assert!(check_host("example.com", &config).is_ok());
    }

    #[test]
    fn allowlisted_segments_override_the_block() {
        let mut config = enabled();
        config.ssrf_policy.allowed_segments = vec!["10.0.0.0/8".to_owned()];
        assert!(check_host("10.0.1.1", &config).is_ok());
    }

    #[test]
    fn ipv4_mapped_ipv6_literals_are_normalised() {
        let config = enabled();
        assert!(check_host("::ffff:127.0.0.1", &config).is_err());
        assert!(check_host("::ffff:10.0.1.1", &config).is_err());
        assert!(check_host("[::ffff:192.168.1.5]", &config).is_err());
        assert!(check_host("::ffff:example.com", &config).is_ok());
    }

    #[test]
    fn ipv4_compatible_ipv6_literals_are_normalised() {
        let config = enabled();
        // `::7f00:1` is 127.0.0.1 spelled as an IPv4-compatible IPv6 address.
        assert!(check_host("::7f00:1", &config).is_err());
        // `::0a00:101` is 10.0.1.1.
        assert!(check_host("::0a00:101", &config).is_err());
        // `::1` stays IPv6 loopback and is blocked on its own merits.
        assert!(check_host("::1", &config).is_err());
    }

    #[test]
    fn hostname_matches_respect_label_boundaries() {
        let mut config = enabled();
        config.ssrf_policy.blocked_segments = vec!["internal".to_owned()];
        assert!(check_host("api.internal", &config).is_err());
        assert!(check_host("internal", &config).is_err());
        // A suffix that only looks like a match is not one.
        assert!(check_host("notinternal.example", &config).is_ok());
    }

    #[test]
    fn blocked_cidrs_apply_to_literals_and_out_of_range_masks_are_ignored() {
        let mut config = enabled();
        config.ssrf_policy.blocked_segments =
            vec!["9.9.9.0/24".to_owned(), "9.9.0.0/40".to_owned()];
        assert!(check_host("9.9.9.9", &config).is_err());
        assert!(check_host("9.9.1.1", &config).is_ok());
        // `/40` cannot describe an IPv4 network, so that entry is unusable and
        // must not widen the block to the whole /8.
        assert!(check_host("9.9.0.1", &config).is_ok());
    }

    #[test]
    fn explicit_block_list_wins_over_dns_names() {
        let mut config = enabled();
        config.ssrf_policy.blocked_segments = vec!["evil.internal".to_owned()];
        assert!(check_host("evil.internal", &config).is_err());
    }
}
