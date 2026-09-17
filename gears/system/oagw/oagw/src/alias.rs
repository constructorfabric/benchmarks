//! Upstream alias derivation and validation.
//!
//! Alias rules (upstream.v1.schema.json `alias` description):
//!
//! * single hostname with a standard port for its scheme
//!   (http:80, https/wss/wt/grpc:443) → the hostname;
//! * single hostname with a non-standard port → `hostname:port`;
//! * multiple hostnames sharing a common dot suffix (≥2 labels) at the
//!   standard port → the common suffix (e.g. `us.vendor.com` +
//!   `eu.vendor.com` → `vendor.com`);
//! * that common suffix plus a non-standard port → `suffix:port`;
//! * IP-based endpoints cannot auto-derive an alias — an explicit alias is
//!   required;
//! * normalized to ASCII lowercase with trailing dots stripped.
//!
//! Derived aliases must additionally resolve as a valid PSL registrable
//! domain (hostname case) so multi-tenant shadowing cannot collide on an
//! unverifiable label.

use crate::gts;

/// Scheme → default port mapping (https-family defaults to 443).
fn default_port(scheme: &str) -> u16 {
    match scheme {
        "http" => 80,
        _ => 443,
    }
}

/// Normalize a host: lowercase, strip a single trailing dot.
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn is_ip_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Whether `candidate` is a valid alias spelling.
pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty() {
        return false;
    }
    let bytes = alias.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-'))
}

/// Derive the common dot-suffix of `a` and `b` with ≥2 labels, if any.
fn common_suffix(a: &str, b: &str) -> Option<String> {
    let a_labels: Vec<&str> = a.split('.').collect();
    let b_labels: Vec<&str> = b.split('.').collect();
    let mut labels: Vec<&str> = Vec::new();
    for (x, y) in a_labels.iter().rev().zip(b_labels.iter().rev()) {
        if x != y {
            break;
        }
        labels.push(x);
    }
    if labels.len() < 2 {
        return None;
    }
    labels.reverse();
    Some(labels.join("."))
}

/// Derive the alias for a set of `(scheme, host, port)` endpoints.
///
/// Returns `None` when the alias cannot be derived (IP endpoint present,
/// heterogeneous ports, or no common suffix) — callers then require an
/// explicit alias.
pub fn derive_alias(endpoints: &[(String, String, Option<u16>)]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    let normalized: Vec<(String, String, u16)> = endpoints
        .iter()
        .map(|(scheme, host, port)| {
            let s = scheme.to_ascii_lowercase();
            let h = normalize_host(host);
            let p = port.unwrap_or_else(|| default_port(&s));
            (s, h, p)
        })
        .collect();

    if normalized.iter().any(|(_, h, _)| is_ip_host(h)) {
        return None;
    }

    let first = &normalized[0];
    let all_same_port = normalized
        .iter()
        .all(|(_, _, p)| *p == first.2);
    if !all_same_port {
        return None;
    }

    // Distinct hostnames.
    let mut hosts: Vec<&str> = normalized.iter().map(|(_, h, _)| h.as_str()).collect();
    hosts.sort();
    hosts.dedup();

    let base: String = if hosts.len() == 1 {
        hosts[0].to_string()
    } else {
        let mut suffix: Option<String> = None;
        for &h in &hosts {
            suffix = match suffix {
                None => Some(h.to_string()),
                Some(cur) => common_suffix(&cur, h),
            };
        }
        suffix?
    };

    let port = first.2;
    let standard = port == default_port(&first.0);
    let alias = if standard {
        base
    } else {
        format!("{base}:{port}")
    };

    Some(alias)
}

/// Validate a derived-or-explicit alias.
///
/// Hostname-style aliases (no `:port`) must form a valid PSL registrable
/// domain unless they are the reserved plain-hostname form already proven
/// by endpoint type; explicit aliases on IP-based upstreams are exempt from
/// PSL validation. `ip_like` disables the PSL check for IP upstreams.
pub fn validate_alias(alias: &str, ip_like: bool) -> bool {
    if !is_valid_alias(alias) {
        return false;
    }
    if ip_like {
        return true;
    }
    // `hostname:port` form — validate the hostname portion.
    let host = match alias.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => alias,
    };
    if host.is_empty() {
        return false;
    }
    psl::domain_str(host).is_some()
}

/// GTS protocol → simple scheme-allowlist validation.
pub fn scheme_is_proxyable(scheme: &str) -> bool {
    matches!(scheme, "https" | "http" | "wss" | "wt" | "grpc")
}

/// Is this the reserved protocol identifier for gRPC upstreams?
pub fn is_grpc_protocol(protocol: &str) -> bool {
    protocol == gts::PROTOCOL_GRPC
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn single_hostname_standard_port() {
        let eps = vec![("https".into(), "api.openai.com".into(), None)];
        assert_eq!(derive_alias(&eps).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_non_standard_port() {
        let eps = vec![("https".into(), "api.example.com".into(), Some(8443))];
        assert_eq!(derive_alias(&eps).as_deref(), Some("api.example.com:8443"));
    }

    #[test]
    fn http_standard_port_is_80() {
        let eps = vec![("http".into(), "svc.local".into(), None)];
        assert_eq!(derive_alias(&eps).as_deref(), Some("svc.local"));
    }

    #[test]
    fn common_suffix_two_hosts() {
        let eps = vec![
            ("https".into(), "us.vendor.com".into(), None),
            ("https".into(), "eu.vendor.com".into(), None),
        ];
        assert_eq!(derive_alias(&eps).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn common_suffix_non_standard_port() {
        let eps = vec![
            ("https".into(), "us.vendor.com".into(), Some(8443)),
            ("https".into(), "eu.vendor.com".into(), Some(8443)),
        ];
        assert_eq!(derive_alias(&eps).as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn heterogeneous_ports_not_derivable() {
        let eps = vec![
            ("https".into(), "a.example.com".into(), None),
            ("https".into(), "b.example.com".into(), Some(8443)),
        ];
        assert!(derive_alias(&eps).is_none());
    }

    #[test]
    fn ip_endpoints_require_explicit_alias() {
        let eps = vec![("https".into(), "10.0.0.1".into(), None)];
        assert!(derive_alias(&eps).is_none());
    }

    #[test]
    fn alias_validation() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("a1-b2.example-3.com:8443"));
        assert!(!is_valid_alias("-bad"));
        assert!(!is_valid_alias("bad_"));
        assert!(!is_valid_alias("UPPER.case"));
        assert!(validate_alias("api.openai.com", false));
        assert!(validate_alias("10.0.0.1", true));
        assert!(!validate_alias("not a host", false));
    }
}
