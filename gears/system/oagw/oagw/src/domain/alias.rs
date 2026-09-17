//! Alias derivation and validation (ADR 0001, upstream.v1.schema.json).
//!
//! The routing alias is derived from the upstream endpoints:
//!
//! - single hostname with standard port (http:80, https/wss/wt/grpc:443)
//!   → `hostname`;
//! - single hostname with non-standard port → `hostname:port`;
//! - multiple hostnames sharing a common registrable suffix (≥2 labels,
//!   PSL-validated) with a standard port → common suffix; non-standard shared
//!   port → `suffix:port` (these require `X-OAGW-Target-Host` at proxy time);
//! - IP-address endpoints or non-derivable host sets → explicit alias
//!   required (a `MissingTargetHost`-style 400 at creation unless provided).
//!
//! Aliases normalize to ASCII lowercase with trailing dots stripped, and are
//! immutable once set.

use std::net::IpAddr;

use super::models::Endpoint;

/// Result of alias derivation for a set of endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasInfo {
    /// Derived alias when derivable, else `None` (explicit alias required).
    pub derived: Option<String>,
    /// Whether the derived alias represents a multi-host common-suffix set:
    /// proxies must then carry `X-OAGW-Target-Host` to disambiguate.
    pub target_host_required: bool,
    /// Multi-host set that is *not* derivable (no common registrable suffix):
    /// an explicit alias is required, and `X-OAGW-Target-Host` is optional
    /// (round-robin across endpoints).
    pub multi_explicit: bool,
}

/// Derive alias metadata from a non-empty endpoint list.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> AliasInfo {
    if endpoints.is_empty() {
        return AliasInfo {
            derived: None,
            target_host_required: false,
            multi_explicit: false,
        };
    }

    // Distinct normalized hosts, preserving first-seen order.
    let mut hosts: Vec<String> = Vec::new();
    for e in endpoints {
        let h = e.normalized_host();
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    }

    // Any IP endpoint makes derivation impossible.
    if hosts.iter().any(|h| h.parse::<IpAddr>().is_ok()) {
        return AliasInfo {
            derived: None,
            target_host_required: false,
            multi_explicit: hosts.len() > 1,
        };
    }

    if hosts.len() == 1 {
        // Single hostname: `host` or `host:port` (non-standard only).
        let e = endpoints
            .iter()
            .find(|e| e.normalized_host() == hosts[0])
            .expect("endpoint exists");
        let derived = if e.is_standard_port() {
            hosts[0].clone()
        } else {
            format!("{}:{}", hosts[0], e.port)
        };
        return AliasInfo {
            derived: Some(derived),
            target_host_required: false,
            multi_explicit: false,
        };
    }

    // Multiple hostnames: try common-suffix derivation.
    if let Some(suffix) = common_suffix(&hosts) {
        let labels: Vec<&str> = suffix.split('.').collect();
        // Derivable only when the suffix is a registrable domain with ≥2
        // labels (never a bare public suffix like `com`).
        let psl_valid = labels.len() >= 2 && psl::domain_str(&suffix).is_some();
        if psl_valid {
            // Port handling: shared non-standard port → `suffix:port`;
            // otherwise (all standard, or mixed) keep it host-only.
            let ports: std::collections::BTreeSet<u16> =
                endpoints.iter().map(|e| e.port).collect();
            let scheme = endpoints[0].scheme.as_str();
            let standard_for = |p: u16| match scheme {
                "http" => p == 80,
                _ => p == 443,
            };
            let non_standard_shared = ports.len() == 1 && !standard_for(*ports.iter().next().unwrap());
            let derived = if non_standard_shared {
                format!("{suffix}:{}", ports.iter().next().unwrap())
            } else {
                suffix
            };
            return AliasInfo {
                derived: Some(derived),
                target_host_required: true,
                multi_explicit: false,
            };
        }
    }

    AliasInfo {
        derived: None,
        target_host_required: false,
        multi_explicit: true,
    }
}

/// Longest common dot-separated suffix shared by all `hosts`.
fn common_suffix(hosts: &[String]) -> Option<String> {
    let labelsets: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').collect::<Vec<_>>())
        .collect();
    if labelsets.is_empty() {
        return None;
    }
    let min_len = labelsets.iter().map(|l| l.len()).min()?;
    let mut common: Vec<&str> = Vec::new();
    for i in 0..min_len {
        let label = labelsets[0][labelsets[0].len() - 1 - i];
        if labelsets.iter().all(|l| l[l.len() - 1 - i] == label) {
            common.push(label);
        } else {
            break;
        }
    }
    if common.is_empty() {
        None
    } else {
        Some(common.iter().rev().copied().collect::<Vec<_>>().join("."))
    }
}

/// RFC 1123 hostname validation: total length ≤ 253, labels 1..=63 chars,
/// `[a-zA-Z0-9-]`, no leading/trailing hyphen per label. A single trailing
/// dot is tolerated and stripped by normalization.
#[must_use]
pub fn valid_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Normalize a candidate alias for storage/comparison: ASCII lowercase,
/// trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim_end_matches('.').to_ascii_lowercase()
}

/// Validate user-supplied alias syntax (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`)
/// after normalization (which allows uppercase/trailing-dot input).
#[must_use]
pub fn valid_alias(alias: &str) -> bool {
    let a = normalize_alias(alias);
    let bytes = a.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit())
        || !(last.is_ascii_lowercase() || last.is_ascii_digit())
    {
        return false;
    }
    // The alias must stay a valid authority-like token: hostname[:port].
    if let Some((host, port)) = a.rsplit_once(':') {
        if host.is_empty() || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    a.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b':' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.into(),
            host: host.into(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_no_suffix() {
        let info = derive_alias(&[ep("https", "api.openai.com", 443)]);
        assert_eq!(info.derived.as_deref(), Some("api.openai.com"));
        assert!(!info.target_host_required);
        assert!(!info.multi_explicit);
    }

    #[test]
    fn single_hostname_http_standard_port() {
        let info = derive_alias(&[ep("http", "localhost", 80)]);
        assert_eq!(info.derived.as_deref(), Some("localhost"));
    }

    #[test]
    fn single_hostname_nonstandard_port() {
        let info = derive_alias(&[ep("http", "localhost", 8080)]);
        assert_eq!(info.derived.as_deref(), Some("localhost:8080"));
    }

    #[test]
    fn multi_host_common_suffix_requires_target_host() {
        let info = derive_alias(&[
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 443),
        ]);
        assert_eq!(info.derived.as_deref(), Some("vendor.com"));
        assert!(info.target_host_required);
    }

    #[test]
    fn multi_host_common_suffix_with_port() {
        let info = derive_alias(&[
            ep("http", "us.vendor.com", 8443),
            ep("http", "eu.vendor.com", 8443),
        ]);
        assert_eq!(info.derived.as_deref(), Some("vendor.com:8443"));
        assert!(info.target_host_required);
    }

    #[test]
    fn multi_host_common_suffix_bare_public_suffix_not_derivable() {
        // `a.com` + `b.com` → common suffix `com` (single label, public
        // suffix) → not derivable.
        let info = derive_alias(&[
            ep("https", "a.com", 443),
            ep("https", "b.com", 443),
        ]);
        assert_eq!(info.derived, None);
        assert!(info.multi_explicit);
    }

    #[test]
    fn unrelated_hosts_not_derivable() {
        let info = derive_alias(&[
            ep("https", "one.example.com", 443),
            ep("https", "two.other.org", 443),
        ]);
        assert_eq!(info.derived, None);
        assert!(info.multi_explicit);
    }

    #[test]
    fn ip_endpoints_require_explicit_alias() {
        let info = derive_alias(&[ep("https", "10.0.0.1", 443)]);
        assert_eq!(info.derived, None);
        assert!(!info.multi_explicit);
    }

    #[test]
    fn hostname_validation() {
        assert!(valid_hostname("api.openai.com"));
        assert!(valid_hostname("localhost"));
        assert!(valid_hostname("API.OpenAI.com."));
        assert!(!valid_hostname("-bad.com"));
        assert!(!valid_hostname("bad-.com"));
        assert!(!valid_hostname("a..b"));
        assert!(!valid_hostname(""));
    }

    #[test]
    fn alias_syntax_validation() {
        assert!(valid_alias("api.openai.com"));
        assert!(valid_alias("localhost:8080"));
        assert!(valid_alias("vendor.com:8443"));
        assert!(!valid_alias(""));
        assert!(!valid_alias("-foo"));
        assert!(!valid_alias("foo-"));
        assert!(!valid_alias("foo bar"));
        assert!(!valid_alias("Foo.Bar   "));
    }

    #[test]
    fn alias_normalization_lowercases_and_strips_dots() {
        assert_eq!(normalize_alias("API.OpenAI.COM."), "api.openai.com");
    }
}
