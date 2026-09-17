//! Alias derivation/enforcement rules and the configuration-boundary SSRF
//! guard.
//!
//! Alias rules (schema `alias`): derivation from a server pool's endpoints,
//! normalization (ASCII lowercase, trailing dots stripped), and the alias
//! pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
//!
//! SSRF guard (DoD `cpt-cf-oagw-dod-domain-model-repositories-ssrf`,
//! algorithm `cpt-cf-oagw-algo-domain-model-repositories-ssrf-validate`):
//! scheme allowlist (`https` by default; plaintext `http` only when
//! `allow_http_upstream` is enabled) plus RFC 1123 hostname validation.

use super::config::EndpointScheme;
use super::upstream::Endpoint;

/// Standard default port for a given scheme (HTTP:80, TLS schemes:443).
#[must_use]
pub const fn standard_port(scheme: EndpointScheme) -> Option<u16> {
    match scheme {
        EndpointScheme::Http => Some(80),
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            Some(443)
        }
    }
}

/// Normalizes an alias per the control-plane alias rule
/// (`inst-cp-alias-normalize`): ASCII lowercase and trailing dots stripped.
/// Normalization is idempotent and makes `API.vendor.COM.` ⟷ `api.vendor.com`
/// compare equal for `(tenant_id, alias)` uniqueness.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim_end_matches('.').to_ascii_lowercase()
}

/// Returns the host part of an alias/host, stripping a trailing `:port`
/// suffix when present (derived aliases carry a port, e.g. `vendor.com:8443`).
#[must_use]
pub fn alias_host_part(alias: &str) -> &str {
    match alias.rfind(':') {
        // A colon whose tail is purely digits is a `:port` suffix.
        Some(idx)
            if idx > 0
                && !alias[..idx].ends_with(':')
                && alias[idx + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            &alias[..idx]
        }
        _ => alias,
    }
}

/// Whether `host` is itself a public suffix per the PSL (e.g. `co.uk`,
/// `com.au`) — a "bare public suffix" that is not a registrable domain.
///
/// Used by the alias enforcement rule to reject derived aliases that would
/// collapse an entire public suffix into one owner-shared alias (algorithm
/// `cpt-cf-oagw-algo-control-plane-api-alias-derivation`).
#[must_use]
pub fn is_bare_public_suffix(host: &str) -> bool {
    // `psl::suffix_str` returns the longest matching public suffix; when the
    // whole normalized host is its own suffix the name is a bare public
    // suffix. Unknown single labels fall back to themselves, but derived
    // aliases always carry at least two labels (`compute_derived_alias`).
    matches!(psl::suffix_str(host), Some(s) if s == host)
}

/// Whether a server pool requires the operator to supply an explicit alias
/// instead of a derived one.
///
/// Derivation is refused (and an explicit alias required) when:
/// - the pool is not derivable at all — IP-based endpoints, single-label
///   hosts, or no common suffix (`compute_derived_alias` returns `None`);
/// - the derived alias's host part is a bare public suffix (e.g. `a.co.uk` +
///   `b.co.uk` → `co.uk`), which would be unsafe to share as an owner alias.
#[must_use]
pub fn requires_explicit_alias(endpoints: &[Endpoint]) -> bool {
    match compute_derived_alias(endpoints) {
        None => true,
        Some(alias) => is_bare_public_suffix(alias_host_part(&alias)),
    }
}

/// Validates an upstream alias against the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
/// Returns a description of the invalid alias.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("alias must not be empty".to_owned());
    }
    let bytes = alias.as_bytes();
    // Pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`: the first and last
    // characters must be `[a-z0-9]`; only the interior may carry `.:-`.
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let interior = |b: u8| edge(b) || matches!(b, b'.' | b':' | b'-');
    if !edge(bytes[0]) {
        return Err(format!(
            "alias must start with a lowercase letter or digit: '{alias}'"
        ));
    }
    if !edge(bytes[bytes.len() - 1]) {
        return Err(format!(
            "alias must end with a lowercase letter or digit: '{alias}'"
        ));
    }
    if !bytes.iter().copied().all(interior) {
        return Err(format!(
            "alias contains characters outside [a-z0-9.:-]: '{alias}'"
        ));
    }
    Ok(())
}

/// Validates a host per RFC 1123 hostname rules.
///
/// Accepts:
/// - DNS hostnames: labels of 1-63 chars using `[a-zA-Z0-9-]`, no leading or
///   trailing hyphen per label, total length <= 253, an optional single
///   trailing dot (FQDN);
/// - IP literals (IPv4 / IPv6).
///
/// # Errors
/// Returns a description of why the host is not RFC 1123 conformant.
pub fn validate_rfc1123_hostname(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("host must not be empty".to_owned());
    }

    // IP literals are valid hosts: IPv4 / IPv6 (bare or bracket-enclosed).
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    if let Some(bare) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'))
        && bare.parse::<std::net::Ipv6Addr>().is_ok()
    {
        return Ok(());
    }

    // Strip one optional trailing dot (FQDN form).
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() {
        return Err("host consists of a single trailing dot".to_owned());
    }
    if name.len() > 253 {
        return Err(format!("hostname length {} exceeds 253", name.len()));
    }

    for label in name.split('.') {
        if label.is_empty() {
            return Err(format!(
                "hostname '{host}' contains an empty label (adjacent dots)"
            ));
        }
        if label.len() > 63 {
            return Err(format!("hostname label '{label}' exceeds 63 characters"));
        }
        let ok = label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(format!(
                "hostname label '{label}' must be alphanumeric +/- and not start/end with '-'"
            ));
        }
    }
    Ok(())
}

/// Applies the configuration-boundary SSRF guard to a single endpoint.
///
/// Rules (algorithm `cpt-cf-oagw-algo-domain-model-repositories-ssrf-validate`):
/// 1. scheme must be in the allowlist — TLS schemes (`https`, `wss`, `wt`,
///    `grpc`) always; plaintext `http` only when `allow_http_upstream`;
/// 2. host must conform to RFC 1123 hostname rules.
///
/// # Errors
/// Returns a description of the first violated rule.
pub fn validate_endpoint_url(endpoint: &Endpoint, allow_http_upstream: bool) -> Result<(), String> {
    if !endpoint.scheme.is_tls() && !allow_http_upstream {
        return Err(format!(
            "scheme '{:?}' is not allow-listed: plaintext HTTP upstreams require \
             allow_http_upstream=true",
            endpoint.scheme
        ));
    }
    validate_rfc1123_hostname(&endpoint.host).map_err(|e| format!("endpoint host invalid: {e}"))
}

/// Computes the derived alias for a server pool per the schema derivation
/// rules, returning `None` when the endpoints are not derivable
/// (IP-based endpoints or no common suffix).
///
/// Rules:
/// - single hostname with standard port (HTTP:80, TLS:443) → hostname;
/// - single hostname with non-standard port → `hostname:port`;
/// - multiple hostnames with a common suffix (>= 2 labels) and standard port
///   → the common suffix (e.g. `us.vendor.com` + `eu.vendor.com` →
///   `vendor.com`);
/// - multiple hostnames with a common suffix and non-standard port →
///   `common_suffix:port`;
/// - otherwise (IP-based or non-derivable) → `None`.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }

    // All endpoints must be hostnames with a uniform port class (all standard
    // or all non-standard) for the pool to be derivable.
    let mut first: Option<(EndpointScheme, String, u16)> = None;
    let mut port_class: Option<bool> = None; // true = standard, false = non-standard
    let mut common_suffix: Option<Vec<String>> = None;

    for ep in endpoints {
        // IP-based endpoints are never derivable.
        if matches!(
            url::Host::parse(&ep.host),
            Ok(url::Host::Ipv4(_) | url::Host::Ipv6(_))
        ) {
            return None;
        }
        let host = ep.host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return None;
        }
        let labels: Vec<String> = host.split('.').map(str::to_owned).collect();

        let is_standard = standard_port(ep.scheme) == Some(ep.port);
        match port_class {
            None => {
                port_class = Some(is_standard);
                first = Some((ep.scheme, host, ep.port));
                common_suffix = Some(labels);
            }
            Some(expected) => {
                if expected != is_standard {
                    // Mixed standard/non-standard ports are not derivable together.
                    return None;
                }
                // Common suffix over the running pool.
                let acc = common_suffix.take().unwrap_or_default();
                common_suffix = Some(intersect_suffix(acc, labels));
            }
        }
    }

    let (scheme, _first_host, first_port) = first?;
    let standard = standard_port(scheme) == Some(first_port);
    let suffix = common_suffix.unwrap_or_default();

    // Common suffix must keep at least two labels to act as a derived alias.
    if suffix.len() < 2 {
        return None;
    }
    let mut alias = suffix.join(".");
    if !standard {
        alias.push_str(&format!(":{first_port}"));
    }
    Some(alias)
}

/// Longest common suffix (in labels) between two label lists.
fn intersect_suffix(a: Vec<String>, b: Vec<String>) -> Vec<String> {
    let mut i = a.len();
    let mut j = b.len();
    let mut common: Vec<String> = Vec::new();
    while i > 0 && j > 0 && a[i - 1] == b[j - 1] {
        common.push(a[i - 1].clone());
        i -= 1;
        j -= 1;
    }
    common.reverse();
    common
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn rfc1123_accepts_valid_hostnames() {
        for host in [
            "api.vendor.com",
            "localhost",
            "my-host",
            "xn--bcher-kva.example",
            "api.vendor.com.",
            "10.0.0.1",
            "2001:db8::1",
            "1.2.3.4",
        ] {
            assert!(
                validate_rfc1123_hostname(host).is_ok(),
                "expected {host} to be valid"
            );
        }
    }

    #[test]
    fn rfc1123_rejects_invalid_hostnames() {
        for host in [
            "",
            "-leading",
            "trailing-",
            "under_score",
            "a..b",
            ".lead",
            &"a".repeat(64),
            &format!("{}.com", "a".repeat(64)),
        ] {
            assert!(
                validate_rfc1123_hostname(host).is_err(),
                "expected {host} to be rejected"
            );
        }
        // Single-label names and a single trailing FQDN dot are valid per
        // RFC 1123 / DNS label rules.
        assert!(validate_rfc1123_hostname("a").is_ok());
        assert!(validate_rfc1123_hostname("trail.").is_ok());
    }

    #[test]
    fn ssrf_scheme_allowlist() {
        let https = ep(EndpointScheme::Https, "api.vendor.com", 443);
        assert!(validate_endpoint_url(&https, false).is_ok());
        // TLS-based schemes always allowlisted.
        for scheme in [
            EndpointScheme::Wss,
            EndpointScheme::Wt,
            EndpointScheme::Grpc,
        ] {
            let e = ep(scheme, "api.vendor.com", 443);
            assert!(validate_endpoint_url(&e, false).is_ok());
        }
        // Plaintext http rejected unless explicitly allowed.
        let http = ep(EndpointScheme::Http, "api.vendor.com", 80);
        assert!(validate_endpoint_url(&http, false).is_err());
        assert!(validate_endpoint_url(&http, true).is_ok());
    }

    #[test]
    fn ssrf_rejects_non_rfc1123_host() {
        let e = ep(EndpointScheme::Https, "under_score_host.com", 443);
        assert!(validate_endpoint_url(&e, true).is_err());
        let e = ep(EndpointScheme::Https, "-bad.example", 443);
        assert!(validate_endpoint_url(&e, true).is_err());
    }

    #[test]
    fn alias_pattern_validation() {
        assert!(validate_alias("api.openai.com").is_ok());
        assert!(validate_alias("vendor.com:8443").is_ok());
        assert!(validate_alias("my-alias_2").is_err()); // underscore
        assert!(validate_alias("-lead").is_err());
        assert!(validate_alias("trail-").is_err());
        assert!(validate_alias("UPPER").is_err());
    }

    #[test]
    fn derive_alias_single_hostname_standard_port() {
        let pool = vec![ep(EndpointScheme::Https, "api.vendor.com", 443)];
        assert_eq!(
            compute_derived_alias(&pool).as_deref(),
            Some("api.vendor.com")
        );
    }

    #[test]
    fn derive_alias_single_hostname_nonstandard_port() {
        let pool = vec![ep(EndpointScheme::Https, "api.vendor.com", 8443)];
        assert_eq!(
            compute_derived_alias(&pool).as_deref(),
            Some("api.vendor.com:8443")
        );
    }

    #[test]
    fn derive_alias_common_suffix() {
        let pool = vec![
            ep(EndpointScheme::Https, "us.vendor.com", 443),
            ep(EndpointScheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(compute_derived_alias(&pool).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn derive_alias_common_suffix_nonstandard_port() {
        let pool = vec![
            ep(EndpointScheme::Https, "us.vendor.com", 8443),
            ep(EndpointScheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&pool).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn derive_alias_ip_based_is_not_derivable() {
        let pool = vec![ep(EndpointScheme::Https, "10.0.0.1", 443)];
        assert_eq!(compute_derived_alias(&pool), None);
    }

    #[test]
    fn derive_alias_single_label_not_derivable() {
        let pool = vec![ep(EndpointScheme::Https, "localhost", 443)];
        assert_eq!(compute_derived_alias(&pool), None);
    }

    #[test]
    fn normalize_alias_lowercases_and_strips_dots() {
        assert_eq!(normalize_alias("API.Vendor.COM."), "api.vendor.com");
        assert_eq!(normalize_alias("api.vendor.com"), "api.vendor.com");
        assert_eq!(normalize_alias("Vendor.COM:8443."), "vendor.com:8443");
        // Idempotent.
        assert_eq!(
            normalize_alias(&normalize_alias("API.Vendor.COM.")),
            "api.vendor.com"
        );
    }

    #[test]
    fn alias_host_part_strips_trailing_port() {
        assert_eq!(alias_host_part("vendor.com"), "vendor.com");
        assert_eq!(alias_host_part("vendor.com:8443"), "vendor.com");
        assert_eq!(alias_host_part("vendor.com:443"), "vendor.com");
    }

    #[test]
    fn bare_public_suffix_detection() {
        // Real ICANN public suffixes.
        assert!(is_bare_public_suffix("co.uk"));
        assert!(is_bare_public_suffix("com"));
        // Registrable domains under them are not bare suffixes.
        assert!(!is_bare_public_suffix("vendor.co.uk"));
        assert!(!is_bare_public_suffix("vendor.com"));
        // Empty input is never a bare suffix.
        assert!(!is_bare_public_suffix(""));
    }

    #[test]
    fn requires_explicit_alias_cases() {
        // IP-based pool: not derivable, explicit alias required.
        assert!(requires_explicit_alias(&[ep(
            EndpointScheme::Https,
            "10.0.0.1",
            443
        )]));
        // Single-label host: not derivable.
        assert!(requires_explicit_alias(&[ep(
            EndpointScheme::Https,
            "localhost",
            443
        )]));
        // Bare public suffix as common suffix (a.co.uk + b.co.uk → co.uk).
        assert!(requires_explicit_alias(&[
            ep(EndpointScheme::Https, "a.co.uk", 443),
            ep(EndpointScheme::Https, "b.co.uk", 443),
        ]));
        // Derivable hostname pools never require an explicit alias, including
        // multi-host pools whose common suffix is a registrable domain.
        assert!(!requires_explicit_alias(&[ep(
            EndpointScheme::Https,
            "api.vendor.com",
            443
        )]));
        assert!(!requires_explicit_alias(&[
            ep(EndpointScheme::Https, "us.vendor.com", 443),
            ep(EndpointScheme::Https, "eu.vendor.com", 443),
        ]));
        assert!(!requires_explicit_alias(&[
            ep(EndpointScheme::Https, "us.vendor.co.uk", 443),
            ep(EndpointScheme::Https, "eu.vendor.co.uk", 443),
        ]));
    }
}
