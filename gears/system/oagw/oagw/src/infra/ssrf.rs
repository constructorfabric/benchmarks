//! Server-Side Request Forgery protection (DESIGN "Security Considerations").
//!
//! Runs per proxied request *before* any bytes are sent: the target host is
//! resolved to IPs and each is checked against the configured policy. This is
//! a best-effort application-layer guard; the toolkit-http client performs its
//! own DNS at connect time (a documented residual TOCTOU window). E2E mode
//! disables the checks wholesale via `ssrf_policy.enabled: false`.

use std::net::IpAddr;

use crate::config::SsrfConfig;

/// Outcome of an SSRF check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsrfVerdict {
    /// The target is allowed.
    Allowed,
    /// The target host is not resolvable at all.
    Unresolvable,
    /// A resolved address falls into a blocked range.
    Blocked { address: IpAddr },
}

/// Check a single host against the policy.
///
/// # Errors
///
/// Returns [`SsrfVerdict::Unresolvable`] when the host cannot be resolved and
/// [`SsrfVerdict::Blocked`] when any resolved address is blocked.
pub async fn check_host(config: SsrfConfig, host: &str) -> SsrfVerdict {
    if !config.enabled {
        return SsrfVerdict::Allowed;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_blocked(config, ip) {
            SsrfVerdict::Blocked { address: ip }
        } else {
            SsrfVerdict::Allowed
        };
    }
    // Hostname: resolve via the system resolver and block if ANY address is
    // disallowed (fail-closed when resolution itself fails).
    let mut found = false;
    if let Ok(iter) = tokio::net::lookup_host((host, 0)).await {
        for addr in iter {
            found = true;
            let ip = addr.ip();
            if is_blocked(config, ip) {
                return SsrfVerdict::Blocked { address: ip };
            }
        }
    }
    if found {
        SsrfVerdict::Allowed
    } else {
        SsrfVerdict::Unresolvable
    }
}

fn is_blocked(config: SsrfConfig, ip: IpAddr) -> bool {
    if config.block_loopback && ip.is_loopback() {
        return true;
    }
    if config.block_private && is_private_or_link_local(ip) {
        return true;
    }
    false
}

fn is_private_or_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_loopback()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || octets[0] == 0 // 0.0.0.0/8 (unspecified source / this network)
                || octets[0] == 100 && (octets[1] & 0xC0) == 0x40 // CGNAT 100.64/10
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (`::ffff:a.b.c.d`) and NAT64 (`64:ff9b::a.b.c.d`)
            // addresses embed an IPv4 destination; evaluate the inner address
            // so mapped private/loopback ranges cannot leak through.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_or_link_local(IpAddr::V4(v4));
            }
            if is_nat64_embedded(v6) {
                let octets = v6.octets();
                let v4 = std::net::Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
                return is_private_or_link_local(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
        }
    }
}

/// Whether `v6` carries an embedded IPv4 destination under the NAT64
/// well-known prefix `64:ff9b::/96`.
#[must_use]
fn is_nat64_embedded(v6: std::net::Ipv6Addr) -> bool {
    let octets = v6.octets();
    octets[0] == 0x00
        && octets[1] == 0x64
        && octets[2] == 0xff
        && octets[3] == 0x9b
        && octets[4..12].iter().all(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> SsrfConfig {
        SsrfConfig {
            enabled: true,
            block_loopback: true,
            block_private: true,
        }
    }

    #[tokio::test]
    async fn disabled_policy_allows_everything() {
        let cfg = SsrfConfig {
            enabled: false,
            ..default_config()
        };
        assert_eq!(check_host(cfg, "127.0.0.1").await, SsrfVerdict::Allowed);
        assert_eq!(check_host(cfg, "169.254.1.1").await, SsrfVerdict::Allowed);
    }

    #[tokio::test]
    async fn loopback_is_blocked() {
        assert_eq!(
            check_host(default_config(), "127.0.0.1").await,
            SsrfVerdict::Blocked {
                address: "127.0.0.1".parse().unwrap()
            }
        );
        assert_eq!(
            check_host(default_config(), "::1").await,
            SsrfVerdict::Blocked {
                address: "::1".parse().unwrap()
            }
        );
    }

    #[tokio::test]
    async fn private_ranges_are_blocked() {
        for host in [
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "100.64.0.1",
        ] {
            assert!(
                matches!(
                    check_host(default_config(), host).await,
                    SsrfVerdict::Blocked { .. }
                ),
                "{host} should be blocked"
            );
        }
    }

    #[tokio::test]
    async fn unresolvable_host_is_unresolvable() {
        let verdict = check_host(default_config(), "nonexistent.invalid.host.example").await;
        assert!(matches!(verdict, SsrfVerdict::Unresolvable));
    }

    #[tokio::test]
    async fn ipv4_mapped_private_is_blocked() {
        for host in ["::ffff:127.0.0.1", "::ffff:192.168.1.1", "::ffff:10.1.2.3"] {
            let verdict = check_host(default_config(), host).await;
            assert!(
                matches!(verdict, SsrfVerdict::Blocked { .. }),
                "{host} should be blocked"
            );
        }
        // A mapped public address is not blocked.
        assert_eq!(
            check_host(default_config(), "::ffff:93.184.216.34").await,
            SsrfVerdict::Allowed
        );
    }

    #[tokio::test]
    async fn nat64_embedded_private_is_blocked() {
        // 64:ff9b::a01:101 embeds 10.1.1.1 (blocked); 64:ff9b::5db8:d822
        // embeds 93.184.216.34 (public, allowed).
        assert!(matches!(
            check_host(default_config(), "64:ff9b::a01:101").await,
            SsrfVerdict::Blocked { .. }
        ));
        assert_eq!(
            check_host(default_config(), "64:ff9b::5db8:d822").await,
            SsrfVerdict::Allowed
        );
    }

    #[tokio::test]
    async fn zero_octet_v4_is_blocked() {
        assert!(matches!(
            check_host(default_config(), "0.0.0.0").await,
            SsrfVerdict::Blocked { .. }
        ));
    }
}
