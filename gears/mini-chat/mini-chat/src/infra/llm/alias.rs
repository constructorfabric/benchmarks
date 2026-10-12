//! The OAGW upstream alias of a provider entry: the routing key of `/{alias}{api_path}`.
//!
//! The rules mirror OAGW's alias management (`enforce_alias_create_with` /
//! `enforce_alias_create_derived`), because the alias the resolver routes by must be the alias OAGW
//! registered:
//!
//! - a hostname endpoint has an alias OAGW derives itself: the normalized host (lowercase, trailing
//!   dots stripped), with `:port` appended unless the port is the standard one of the scheme (80
//!   for HTTP, 443 for HTTPS). OAGW rejects any other explicit alias for it, so a configured
//!   `upstream_alias` cannot be registered and the derived alias is used for routing instead;
//! - an IP endpoint has no derived alias: the configured `upstream_alias` (normalized: lowercase,
//!   trailing dots stripped) or else the IP, with `:port` appended unless the port is the standard
//!   one of the scheme (as OAGW derives it for a hostname), is registered explicitly. The port is
//!   part of the default so that two endpoints on one IP (for example two local mocks) do not
//!   share an alias; OAGW accepts `:` in an alias and the proxy parses the alias up to the first
//!   `/`.
//!
//! A tenant override that sets its own host gets the alias of that host. Because OAGW derives the
//! alias of a hostname endpoint, two targets can only be told apart by a different host (or port);
//! provisioning rejects targets that share an alias but not their endpoint and credentials.

use std::net::IpAddr;

use crate::config::{ProviderEntry, TenantOverride};

/// OAGW's host normalization: IPv6 brackets stripped, lowercase, trailing dots stripped.
fn normalize_host(host: &str) -> String {
    let inner = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    inner.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// OAGW's host normalization, for comparing the endpoint of a target with a registered upstream.
#[must_use]
pub fn normalized_host(host: &str) -> String {
    normalize_host(host)
}

/// OAGW's alias normalization of an explicit alias.
fn normalize_alias(alias: &str) -> String {
    alias.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// Whether the host is an IP address, for which OAGW cannot derive an alias and the gear must send
/// an explicit one.
#[must_use]
pub fn is_ip_host(host: &str) -> bool {
    normalize_host(host).parse::<IpAddr>().is_ok()
}

/// The normalized alias of an endpoint: the host, with `:port` unless the port is standard for the
/// scheme. `configured` only counts for an IP host, where it replaces that default.
fn alias_of(host: &str, port: u16, use_http: bool, configured: Option<&str>) -> String {
    let host = normalize_host(host);
    let is_ip = host.parse::<IpAddr>().is_ok();
    let standard = port == if use_http { 80 } else { 443 };
    let endpoint = if standard {
        host
    } else {
        format!("{host}:{port}")
    };
    match configured {
        Some(configured) if is_ip => normalize_alias(configured),
        _ => endpoint,
    }
}

/// The alias of the provider entry itself.
#[must_use]
pub fn upstream_alias(entry: &ProviderEntry) -> String {
    alias_of(
        &entry.host,
        entry.effective_port(),
        entry.use_http,
        entry.upstream_alias.as_deref(),
    )
}

/// The alias a tenant uses: the alias of the override's host (or of the entry's host) with the
/// override's `upstream_alias` taking the place of the entry's, or the provider entry's alias when
/// the override sets neither.
#[must_use]
pub fn upstream_alias_for_tenant(
    entry: &ProviderEntry,
    tenant_override: Option<&TenantOverride>,
) -> String {
    let Some(over) = tenant_override else {
        return upstream_alias(entry);
    };
    if over.host.is_none() && over.upstream_alias.is_none() {
        return upstream_alias(entry);
    }
    let (host, configured) = match &over.host {
        // The override brings its own host: the entry's alias does not apply to it.
        Some(host) => (host.as_str(), over.upstream_alias.as_deref()),
        None => (
            entry.host.as_str(),
            over.upstream_alias
                .as_deref()
                .or(entry.upstream_alias.as_deref()),
        ),
    };
    alias_of(host, entry.effective_port(), entry.use_http, configured)
}

/// The alias OAGW accepts for a target host: normalized, for comparison with listed upstreams.
#[must_use]
pub fn normalized(alias: &str) -> String {
    normalize_alias(alias)
}
