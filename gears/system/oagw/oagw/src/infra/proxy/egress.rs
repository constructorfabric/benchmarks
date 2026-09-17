//! Connect-time egress policy.
//!
//! The `http` *scheme* is a legal create-time value, so whether a plaintext
//! connection is actually made is decided here, immediately before the dial:
//! [`OagwConfig::allow_http_upstream`] gates plaintext and
//! [`OagwConfig::ssrf_policy`] refuses hosts that sit in a forbidden network
//! range.

use std::net::IpAddr;

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Scheme};

/// Whether `endpoint` may be dialled under `config`'s egress policy.
///
/// # Errors
///
/// Returns `cf.oagw.link.unavailable.v1` when the endpoint is plaintext and
/// `allow_http_upstream` is off, or when its host sits in a network range the
/// SSRF policy forbids.
pub async fn gate(config: &OagwConfig, endpoint: &Endpoint) -> Result<(), DomainError> {
    if endpoint.scheme == Scheme::Http && !config.allow_http_upstream {
        return Err(DomainError::link_unavailable(
            "plaintext upstream connections are disabled by `allow_http_upstream`",
        ));
    }
    private_range_gate(&config.ssrf_policy, &endpoint.host).await
}

/// Refuse a host that resolves into a forbidden network range.
///
/// IP literals are classified directly; a hostname is resolved first so an
/// innocuous name pointing at loopback is refused as well.
async fn private_range_gate(policy: &SsrfPolicy, host: &str) -> Result<(), DomainError> {
    if !policy.enabled {
        return Ok(());
    }
    let literal = crate::domain::model::parse_ip(host);
    let forbidden = match literal {
        Some(address) => forbidden(address),
        None => resolves_into_forbidden_range(host).await?,
    };
    if forbidden {
        return Err(DomainError::link_unavailable(format!(
            "upstream host `{host}` is in a forbidden network range"
        )));
    }
    Ok(())
}

/// Resolve `host` and report whether any address is forbidden.
///
/// A name that cannot be resolved is refused too: the gateway has no address
/// to vouch for.
async fn resolves_into_forbidden_range(host: &str) -> Result<bool, DomainError> {
    let resolution = tokio::net::lookup_host((host, 0_u16)).await;
    let mut addresses = resolution.map_err(|error| {
        DomainError::link_unavailable(format!("upstream host `{host}` does not resolve: {error}"))
    })?;
    Ok(addresses.any(|socket| forbidden(socket.ip())))
}

/// Whether `address` is loopback, private, link-local, multicast or
/// unspecified — every range a caller must not be able to make the gateway
/// dial.
fn forbidden(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => v4_forbidden(address),
        IpAddr::V6(address) => match address.to_canonical() {
            // `::ffff:127.0.0.1` is loopback however it is spelled.
            IpAddr::V4(mapped) => v4_forbidden(mapped),
            IpAddr::V6(address) => v6_forbidden(address),
        },
    }
}

/// The IPv4 ranges the SSRF policy refuses.
fn v4_forbidden(address: std::net::Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_unspecified()
}

/// The IPv6 ranges the SSRF policy refuses.
fn v6_forbidden(address: std::net::Ipv6Addr) -> bool {
    address.is_loopback()
        || address.is_multicast()
        || address.is_unspecified()
        || address.is_unicast_link_local()
        || address.is_unique_local()
}

#[cfg(test)]
#[path = "egress_tests.rs"]
mod egress_tests;
