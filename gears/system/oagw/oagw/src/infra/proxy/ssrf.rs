//! Outbound scheme and address screening.
//!
//! Two independent gates sit on the dial path:
//!
//! 1. **scheme gating** — a `http` endpoint is only dialled when
//!    `oagw.config.allow_http_upstream` is `true`. `http` stays a legal
//!    *configuration* value; this gate only governs whether a cleartext
//!    connection is produced (`docs/DESIGN.md` — constraint
//!    `cpt-cf-oagw-constraint-https-only`).
//! 2. **SSRF screening** — when `ssrf_policy.enabled` is `true`, the resolved
//!    target is checked against loopback, link-local, private, and
//!    otherwise-unsuitable address space before any byte is written.
//!
//! Screening happens *after* DNS resolution, on the actual socket address, so
//! a DNS name that resolves into private address space is caught.
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::domain::model::EndpointScheme;
use crate::infra::proxy::failure::ProxyFailure;

/// Outbound screening policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SsrfPolicy {
    /// Screen dial targets when `true`.
    pub enabled: bool,
}

impl SsrfPolicy {
    /// Policy that screens nothing.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }
}

/// Why a dial target was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DialRefusal {
    /// The endpoint scheme is plaintext and plaintext is not permitted.
    #[error(
        "plaintext http upstreams are disabled on this gateway \
         (oagw.config.allow_http_upstream is false)"
    )]
    SchemeDisallowed {
        /// The refused endpoint.
        host: String,
    },
    /// The dial target resolved into screened address space.
    #[error("upstream address {address} is refused by the ssrf policy ({reason})")]
    Screened {
        /// The offending socket address.
        address: String,
        /// Which class of address it belongs to.
        reason: &'static str,
    },
}

/// Whether a plaintext connection to `scheme` is allowed.
#[must_use]
pub const fn plaintext_allowed(scheme: EndpointScheme, allow_http_upstream: bool) -> bool {
    !scheme.is_plaintext() || allow_http_upstream
}

/// Screen a resolved socket address against the SSRF policy.
///
/// # Errors
///
/// [`DialRefusal::Screened`] when the address falls into a class the policy
/// refuses.
pub fn screen_address(address: std::net::IpAddr, policy: SsrfPolicy) -> Result<(), DialRefusal> {
    if !policy.enabled {
        return Ok(());
    }
    if let Some(reason) = classify(address) {
        return Err(DialRefusal::Screened {
            address: address.to_string(),
            reason,
        });
    }
    Ok(())
}

/// The screened class of an address, or `None` when it is dialable.
fn classify(address: std::net::IpAddr) -> Option<&'static str> {
    match address {
        std::net::IpAddr::V4(v4) => classify_v4(v4),
        std::net::IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(address: Ipv4Addr) -> Option<&'static str> {
    let octets = address.octets();
    match address {
        _ if address.is_loopback() => Some("loopback"),
        _ if address.is_private() => Some("private"),
        _ if address.is_link_local() => Some("link-local"),
        _ if address.is_broadcast() => Some("broadcast"),
        _ if address.is_unspecified() => Some("unspecified"),
        _ if octets_start_with(octets, &[100, 64]) && (octets[2] & 0b1100_0000) == 0b0000_0000 => {
            Some("shared-address-space")
        }
        _ if octets_start_with(octets, &[198, 18]) || octets_start_with(octets, &[198, 19]) => {
            Some("benchmarking")
        }
        _ if octets_start_with(octets, &[192, 0, 0]) => Some("ietf-protocol-assignments"),
        _ if octets_start_with(octets, &[192, 0, 2]) => Some("documentation"),
        _ if octets_start_with(octets, &[198, 51, 100]) => Some("documentation"),
        _ if octets_start_with(octets, &[203, 0, 113]) => Some("documentation"),
        _ if octets_start_with(octets, &[169, 254]) => Some("link-local"),
        _ if octets_start_with(octets, &[0, 0, 0]) => Some("this-network"),
        _ if address.is_documentation() => Some("documentation"),
        _ => None,
    }
}

fn classify_v6(address: Ipv6Addr) -> Option<&'static str> {
    let segments = address.segments();
    if address.is_loopback() {
        return Some("loopback");
    }
    if address.is_unspecified() {
        return Some("unspecified");
    }
    // IPv4-mapped and IPv4-compatible addresses are screened on their v4 half.
    if let Some(v4) = address.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    // fc00::/7 — unique local addresses.
    if (segments[0] & 0xfe00) == 0xfc00 {
        return Some("unique-local");
    }
    // fe80::/10 — link-local.
    if (segments[0] & 0xffc0) == 0xfe80 {
        return Some("link-local");
    }
    // ::ffff:0:0/96 and 2001:db8::/32.
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return Some("documentation");
    }
    None
}

fn octets_start_with(octets: [u8; 4], prefix: &[u8]) -> bool {
    octets
        .iter()
        .zip(prefix)
        .all(|(byte, expected)| byte == expected)
}

/// Turn a dial refusal into a 400/403-shaped proxy failure.
///
/// A refused scheme is a request the gateway will not serve (400); a screened
/// address is an internal policy refusal (403).
#[must_use]
pub fn refusal_failure(host: &str, refusal: &DialRefusal) -> ProxyFailure {
    match refusal {
        DialRefusal::SchemeDisallowed { .. } => ProxyFailure::validation(format!(
            "upstream host '{host}' declares a plaintext scheme this gateway does not dial"
        ))
        .with_context("reason", serde_json::json!("scheme_not_allowed")),
        DialRefusal::Screened { address, reason } => ProxyFailure::new(
            403,
            crate::domain::plugin::LINK_UNAVAILABLE,
            "Link Unavailable",
            format!("upstream address '{address}' is refused by the outbound policy"),
        )
        .with_context("reason", serde_json::json!(reason)),
    }
}

#[cfg(test)]
#[path = "ssrf_tests.rs"]
mod tests;
