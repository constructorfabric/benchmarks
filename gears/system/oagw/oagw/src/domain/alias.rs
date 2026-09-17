//! Alias derivation, normalization, and endpoint host validation
//! (`DESIGN.md` §3.2 "Alias Resolution").

use std::net::IpAddr;

/// Maximum length of an RFC 1123 hostname.
pub const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of an RFC 1123 hostname label.
pub const MAX_LABEL_LEN: usize = 63;

/// Normalize an alias or hostname: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize(input: &str) -> String {
    let trimmed = input.trim();
    strip_trailing_dot(trimmed).to_ascii_lowercase()
}

fn strip_trailing_dot(input: &str) -> &str {
    input.strip_suffix('.').unwrap_or(input)
}

/// Validate an alias against the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return false;
    }
    bytes.iter().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b':' || *b == b'-'
    })
}

/// Validate an endpoint host: RFC 1123 hostname or an IP literal.
///
/// Returns `None` when the host is acceptable, otherwise a short reason.
#[must_use]
pub fn validate_host(host: &str) -> Option<String> {
    if host.is_empty() {
        return Some("host must not be empty".to_owned());
    }
    if host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let body = strip_trailing_dot(host);
    if body.is_empty() {
        return Some("host must not be empty".to_owned());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Some(format!("host exceeds {MAX_HOSTNAME_LEN} characters"));
    }
    for label in body.split('.') {
        if label.is_empty() {
            return Some("host contains an empty label".to_owned());
        }
        if label.len() > MAX_LABEL_LEN {
            return Some(format!("host label '{label}' exceeds 63 characters"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Some(format!(
                "host label '{label}' may not start or end with '-'"
            ));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Some(format!("host label '{label}' contains invalid characters"));
        }
    }
    None
}

/// Whether `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Whether `host` names an address a gateway must not dial.
///
/// Both spellings of the loopback name count: `localhost` is a bare RFC 1123
/// label that every resolver answers with a loopback address, so it is rejected
/// without being resolved.
#[must_use]
pub fn is_private_ip(host: &str) -> bool {
    let name = normalize(host);
    if name == "localhost" {
        return true;
    }
    name.parse::<IpAddr>().is_ok_and(is_private_address)
}

/// Whether `ip` is an address a gateway must not dial: loopback, private,
/// link-local, CGNAT, or the unspecified and documentation ranges.
///
/// An IPv4 address wrapped in IPv6 (`::ffff:10.0.0.5`) is judged on the IPv4
/// address it carries: the wrapper changes the spelling, not the network.
#[must_use]
pub fn is_private_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || is_cgnat(v4)
        }
        // A mapped address is judged on the IPv4 address it embeds.
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_address(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

/// Whether `v4` is in the shared-address space carriers hand out behind a NAT
/// (`100.64.0.0/10`, RFC 6598): reachable only inside the carrier's network.
fn is_cgnat(v4: std::net::Ipv4Addr) -> bool {
    let [octet, second, _, _] = v4.octets();
    octet == 100 && (second & 0b1100_0000) == 0b0100_0000
}

/// Derive the alias for an endpoint pool.
///
/// `Some(alias)` when the pool is derivable: a single hostname, or a set of
/// hostnames sharing a registrable common suffix (PSL-validated, at least two
/// labels). `None` when an explicit alias is required.
#[must_use]
pub fn derive_alias(endpoints: &[crate::domain::model::Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if !endpoints.iter().all(|endpoint| !is_ip(&endpoint.host))
        || !endpoints
            .iter()
            .all(|endpoint| validate_host(&endpoint.host).is_none())
    {
        return None;
    }

    let first = endpoints.first()?;
    let port = first.effective_port();
    let standard = first.uses_standard_port();
    let consistent = endpoints
        .iter()
        .all(|endpoint| endpoint.effective_port() == port);
    if !consistent {
        return None;
    }

    let hosts: Vec<String> = endpoints
        .iter()
        .map(|endpoint| normalize(&endpoint.host))
        .collect();

    if hosts.len() == 1 {
        let mut alias = hosts[0].clone();
        if !standard {
            alias.push(':');
            alias.push_str(&port.to_string());
        }
        return Some(alias);
    }

    let suffix = common_suffix(&hosts)?;
    if !is_registrable(&suffix) {
        return None;
    }
    let mut alias = suffix;
    if !standard {
        alias.push(':');
        alias.push_str(&port.to_string());
    }
    Some(alias)
}

/// Longest common dot-suffix shared by every host, at least two labels long.
fn common_suffix(hosts: &[String]) -> Option<String> {
    let first = hosts.first()?;
    if first.is_empty() {
        return None;
    }
    let mut candidate = first.clone();
    for host in &hosts[1..] {
        loop {
            let boundary = is_suffix_boundary(host, &candidate);
            if boundary {
                break;
            }
            if !candidate.contains('.') {
                return None;
            }
            candidate = candidate
                .split_once('.')
                .map_or(String::new(), |(_, rest)| rest.to_owned());
            if candidate.is_empty() {
                return None;
            }
        }
    }
    if candidate.split('.').count() < 2 {
        return None;
    }
    Some(candidate)
}

/// Whether `host` ends with `.candidate` or equals it.
fn is_suffix_boundary(host: &str, candidate: &str) -> bool {
    host == candidate || host.ends_with(&format!(".{candidate}"))
}

/// Whether `name` is a registrable domain per the public suffix list (at least
/// two labels, and not a bare public suffix such as `co.uk`).
fn is_registrable(name: &str) -> bool {
    psl::domain_str(name).is_some_and(|registrable| registrable == name)
}

/// Whether `alias` is a common-suffix alias of the endpoint pool: every host is
/// a subdomain of (or equal to) the alias.
#[must_use]
pub fn is_common_suffix_alias(alias: &str, endpoints: &[crate::domain::model::Endpoint]) -> bool {
    if endpoints.len() < 2 {
        return false;
    }
    let normalized = normalize(alias);
    endpoints
        .iter()
        .all(|endpoint| is_suffix_boundary(&normalize(&endpoint.host), &normalized))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::net::IpAddr;

    use super::{
        common_suffix, derive_alias, is_private_ip, is_registrable, normalize, valid_alias,
    };
    use crate::domain::model::{Endpoint, EndpointScheme};

    fn ep(host: &str, port: u16, scheme: EndpointScheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn normalizes_case_and_trailing_dot() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn single_hostname_derives_host_alias() {
        let endpoints = vec![ep("api.openai.com", 443, EndpointScheme::Https)];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn non_standard_port_is_kept() {
        let endpoints = vec![ep("api.openai.com", 8443, EndpointScheme::Https)];
        assert_eq!(
            derive_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn common_suffix_is_derived() {
        let endpoints = vec![
            ep("us.vendor.com", 443, EndpointScheme::Https),
            ep("eu.vendor.com", 443, EndpointScheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let endpoints = vec![
            ep("foo.co.uk", 443, EndpointScheme::Https),
            ep("bar.co.uk", 443, EndpointScheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
        assert!(!is_registrable("co.uk"));
        assert!(is_registrable("vendor.com"));
    }

    #[test]
    fn ips_require_explicit_alias() {
        let endpoints = vec![
            ep("10.0.1.1", 443, EndpointScheme::Https),
            ep("10.0.1.2", 443, EndpointScheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
        assert!(is_private_ip("10.0.1.1"));
    }

    #[test]
    fn the_loopback_name_is_private_without_being_resolved() {
        for spelling in ["localhost", "LOCALHOST", "localhost."] {
            assert!(is_private_ip(spelling), "{spelling} is the loopback name");
        }
    }

    #[test]
    fn an_ipv4_address_wrapped_in_ipv6_is_judged_on_the_address_it_carries() {
        for wrapped in ["::ffff:10.0.0.5", "::FFFF:192.168.1.1", "::ffff:127.0.0.1"] {
            assert!(is_private_ip(wrapped), "{wrapped} hides a private address");
            let parsed = wrapped.parse::<IpAddr>().expect("a literal");
            assert!(super::is_private_address(parsed));
        }
        assert!(
            !is_private_ip("::ffff:8.8.8.8"),
            "a mapped public address stays public"
        );
    }

    #[test]
    fn unicast_link_local_and_cgnat_are_private() {
        // fe80::/10, unicast link-local (the multicast spelling fe80::/16 with
        // the ip-mapped flag clear is still link-local).
        for address in ["fe80::1", "febf::1", "fe80:0:0:0:0:0:0:1"] {
            assert!(is_private_ip(address), "{address} is link-local");
        }
        // 100.64.0.0/10 (RFC 6598 carrier NAT).
        for address in ["100.64.0.1", "100.100.0.1", "100.127.255.254"] {
            assert!(is_private_ip(address), "{address} is CGNAT space");
        }
        assert!(!is_private_ip("100.128.0.1"), "outside the /10");
        assert!(!is_private_ip("100.63.255.255"), "outside the /10");
    }

    #[test]
    fn a_name_that_cannot_be_an_address_is_not_private() {
        assert!(
            !is_private_ip("api.openai.com"),
            "names are resolved elsewhere"
        );
        assert!(
            !is_private_ip(""),
            "an empty host is a validation failure, not a policy one"
        );
    }

    #[test]
    fn public_addresses_are_never_private() {
        for address in ["8.8.8.8", "2606:4700::1111", "2001:4860:4860::8888"] {
            assert!(!is_private_ip(address), "{address} is public");
        }
    }

    #[test]
    fn common_suffix_helper() {
        assert_eq!(
            common_suffix(&["us.vendor.com".to_owned(), "eu.vendor.com".to_owned()]),
            Some("vendor.com".to_owned())
        );
        assert_eq!(
            common_suffix(&["us.foo.com".to_owned(), "eu.bar.com".to_owned()]),
            None
        );
    }

    #[test]
    fn alias_pattern() {
        assert!(valid_alias("api.openai.com"));
        assert!(valid_alias("my-service"));
        assert!(!valid_alias("-lead"));
        assert!(!valid_alias(""));
        assert!(!valid_alias("Has-Uppercase"));
    }
}
