//! Alias derivation, normalization, and immutability
//! (`cpt-cf-oagw-dod-alias-derivation-normalization`,
//! `cpt-cf-oagw-dod-alias-immutability`).
//!
//! Mirrors DESIGN.md's `common_domain_suffix()` / `enforce_alias_update_*`
//! description: a hostname endpoint's alias is always derived; an
//! IP-addressed or heterogeneous-hostname pool requires an explicit alias.

use std::net::IpAddr;

use super::model::Endpoint;

/// `true` when `host` parses as an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// RFC 1123 hostname validation (`cpt-cf-oagw-dod-schema-validation`): total
/// length at most 253 characters (after stripping one tolerated trailing
/// dot), each label 1-63 characters of ASCII alphanumerics or hyphens, and
/// no label starting or ending with a hyphen.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() || trimmed.len() > 253 {
        return false;
    }
    trimmed.split('.').all(is_valid_label)
}

fn is_valid_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let bytes = label.as_bytes();
    let all_allowed = label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    all_allowed && bytes[0] != b'-' && bytes[bytes.len() - 1] != b'-'
}

/// `true` when `host` is either a valid hostname or an IP literal
/// (`cpt-cf-oagw-dod-schema-validation`: `server.endpoints[].host` must
/// match the hostname, IPv4, or IPv6 format).
#[must_use]
pub fn is_valid_host(host: &str) -> bool {
    is_ip_literal(host) || is_valid_hostname(host)
}

/// Strips one trailing dot and lowercases every ASCII character
/// (`cpt-cf-oagw-dod-alias-derivation-normalization`,
/// `cpt-cf-oagw-algo-alias-normalization`).
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-alias-normalize-impl-01
    let stripped = alias.strip_suffix('.').unwrap_or(alias);
    stripped.to_ascii_lowercase()
    // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-alias-normalize-impl-01
}

/// `true` when `alias` matches the schema's
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` pattern.
#[must_use]
pub fn matches_alias_pattern(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let is_edge_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let is_middle_char = |b: u8| is_edge_char(b) || matches!(b, b'.' | b':' | b'-');
    if !is_edge_char(bytes[0]) {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    if !is_edge_char(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes[1..bytes.len() - 1].iter().all(|b| is_middle_char(*b))
}

/// `true` when `tag` matches the schema's `^[a-z0-9_-]+$` pattern.
#[must_use]
pub fn matches_tag_pattern(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Outcome of alias derivation
/// (`cpt-cf-oagw-algo-alias-derivation`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation {
    /// A single hostname, or a shared registrable-domain suffix, was
    /// derived (already normalized).
    Derived(String),
    /// IP endpoints, heterogeneous hostnames, or a bare public suffix: an
    /// explicit alias is required.
    NotDerivable,
}

/// Computes the registrable-domain suffix shared by every endpoint host, or
/// [`Derivation::NotDerivable`] when the endpoints are IP-addressed,
/// heterogeneous, or share only a bare public suffix
/// (`cpt-cf-oagw-algo-alias-derivation`, `cpt-cf-oagw-dod-alias-derivation-normalization`).
#[must_use]
pub fn derive_root_alias(endpoints: &[Endpoint]) -> Derivation {
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-alias-derive-impl-01
    if endpoints.iter().any(|e| is_ip_literal(&e.host)) {
        return Derivation::NotDerivable;
    }

    let mut hosts: Vec<String> = endpoints.iter().map(|e| normalize_alias(&e.host)).collect();
    hosts.sort_unstable();
    hosts.dedup();

    match hosts.as_slice() {
        [] => Derivation::NotDerivable,
        [only] => Derivation::Derived(only.clone()),
        multiple => derive_common_registrable_domain(multiple),
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-alias-derive-impl-01
}

/// Computes the shared registrable domain (PSL eTLD+1) across every distinct
/// hostname, rejecting derivation when any host has no registrable domain of
/// its own (a bare public suffix) or the registrable domains disagree.
fn derive_common_registrable_domain(hosts: &[String]) -> Derivation {
    let mut domains = hosts.iter().map(|h| psl::domain_str(h));
    let Some(Some(first)) = domains.next() else {
        return Derivation::NotDerivable;
    };
    if domains.all(|d| d == Some(first)) {
        Derivation::Derived(first.to_owned())
    } else {
        Derivation::NotDerivable
    }
}

/// Appends `:{port}` to `root` when `port` is non-standard for `scheme`
/// (`cpt-cf-oagw-algo-alias-derivation` step "shared endpoint port is
/// non-standard").
#[must_use]
pub fn apply_port_suffix(root: &str, endpoints: &[Endpoint]) -> String {
    let Some(first) = endpoints.first() else {
        return root.to_owned();
    };
    let port = first.port.unwrap_or_else(|| first.scheme.standard_port());
    if port == first.scheme.standard_port() {
        root.to_owned()
    } else {
        format!("{root}:{port}")
    }
}

/// Root tenant used as the implicit ancestor for every other tenant.
///
/// This gear does not resolve or reach across the platform's real tenant
/// hierarchy (that walk is out of scope for this feature; see
/// `cpt-cf-oagw-feature-config-resolution`), so the narrow ancestor-disable
/// check this feature's `DoD` requires (`cpt-cf-oagw-dod-enable-disable`) uses
/// the nil UUID tenant as a single, deterministic stand-in "root" ancestor
/// for every other tenant. This is a deliberate, minimal, self-contained
/// approximation — not a new general-purpose hierarchy mechanism — and is
/// called out in the delivery report.
pub const ROOT_TENANT_ID: uuid::Uuid = uuid::Uuid::nil();

#[cfg(test)]
mod tests {
    use super::{
        Derivation, Endpoint, apply_port_suffix, derive_root_alias, is_valid_host,
        is_valid_hostname, matches_alias_pattern, matches_tag_pattern, normalize_alias,
    };
    use crate::domain::model::Scheme;

    fn endpoint(scheme: Scheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_derives_to_itself() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", Some(443))];
        assert_eq!(
            derive_root_alias(&endpoints),
            Derivation::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn shared_registrable_domain_derives_to_common_suffix() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.vendor.com", Some(443)),
            endpoint(Scheme::Https, "eu.vendor.com", Some(443)),
        ];
        assert_eq!(
            derive_root_alias(&endpoints),
            Derivation::Derived("vendor.com".to_owned())
        );
    }

    #[test]
    fn bare_public_suffix_common_part_is_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "foo.co.uk", Some(443)),
            endpoint(Scheme::Https, "bar.co.uk", Some(443)),
        ];
        assert_eq!(derive_root_alias(&endpoints), Derivation::NotDerivable);
    }

    #[test]
    fn heterogeneous_hostnames_are_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.foo.com", Some(443)),
            endpoint(Scheme::Https, "eu.bar.com", Some(443)),
        ];
        assert_eq!(derive_root_alias(&endpoints), Derivation::NotDerivable);
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "10.0.1.1", Some(443)),
            endpoint(Scheme::Https, "10.0.1.2", Some(443)),
        ];
        assert_eq!(derive_root_alias(&endpoints), Derivation::NotDerivable);
    }

    #[test]
    fn non_standard_port_is_appended_to_the_root_alias() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", Some(8443))];
        assert_eq!(
            apply_port_suffix("api.openai.com", &endpoints),
            "api.openai.com:8443"
        );
    }

    #[test]
    fn standard_port_is_not_appended() {
        let endpoints = vec![endpoint(Scheme::Http, "internal.example.com", Some(80))];
        assert_eq!(
            apply_port_suffix("internal.example.com", &endpoints),
            "internal.example.com"
        );
    }

    #[test]
    fn normalization_strips_trailing_dot_and_lowercases() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn hostname_validation_rejects_bad_labels() {
        assert!(is_valid_hostname("api.example.com"));
        assert!(!is_valid_hostname("-bad.example.com"));
        assert!(!is_valid_hostname(""));
    }

    #[test]
    fn host_validation_accepts_ip_or_hostname() {
        assert!(is_valid_host("10.0.0.1"));
        assert!(is_valid_host("::1"));
        assert!(is_valid_host("api.example.com"));
        assert!(!is_valid_host("not a host"));
    }

    #[test]
    fn alias_pattern_rejects_leading_dot_and_uppercase() {
        assert!(matches_alias_pattern("vendor.com"));
        assert!(matches_alias_pattern("a"));
        assert!(!matches_alias_pattern(".vendor.com"));
        assert!(!matches_alias_pattern("Vendor.com"));
        assert!(!matches_alias_pattern(""));
    }

    #[test]
    fn tag_pattern_rejects_uppercase_and_spaces() {
        assert!(matches_tag_pattern("openai"));
        assert!(matches_tag_pattern("llm-v1"));
        assert!(!matches_tag_pattern("Open AI"));
        assert!(!matches_tag_pattern(""));
    }
}
