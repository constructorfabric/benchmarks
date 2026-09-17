//! Upstream alias derivation and normalization.
//!
//! Implements the derivation rules from `docs/DESIGN.md` §3.2
//! (Alias Enforcement Rules): hostname-based endpoints auto-derive an
//! alias; IP-based or non-derivable endpoint sets require an explicit
//! alias from the operator.

use crate::domain::model::{Endpoint, default_port};

/// Normalize an alias / hostname to its canonical form: ASCII
/// lowercase with trailing dots stripped.
pub fn normalize(alias: &str) -> String {
    let lowered: String = alias.to_ascii_lowercase();
    let trimmed = lowered.trim_end_matches('.');
    trimmed.to_owned()
}

/// Whether the string is an IP literal.
pub fn is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Whether the string is a valid RFC 1123 hostname (or an IP literal).
///
/// Max 253 chars total; each label 1–63 chars; labels contain only
/// ASCII alphanumeric and hyphen and cannot start or end with a hyphen.
pub fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if is_ip(host) {
        return true;
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        // Trailing dots are tolerated by callers before validation.
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

/// Compute the derived alias for a set of endpoints per DESIGN §3.2.
///
/// * Single hostname with standard port → hostname.
/// * Single hostname with non-standard port → `hostname:port`.
/// * Multiple hostnames sharing a registrable common suffix (PSL,
///   ≥2 labels, not a bare public suffix) → common suffix, with
///   `:port` appended when the shared port is non-standard.
/// * IP endpoints or non-derivable hostname sets → `None` (explicit
///   alias required).
pub fn derive(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.len() == 1 {
        let e = &endpoints[0];
        if e.is_ip() {
            return None;
        }
        let host = normalize(&e.host);
        if !is_valid_hostname(&host) {
            return None;
        }
        return Some(host_with_port(host, e.resolved_port(), default_port(&e.scheme)));
    }

    // Pool: every endpoint must be a non-IP hostname; all share one
    // scheme and port (pool constraint from DESIGN §3.2).
    let scheme = &endpoints[0].scheme;
    let port = endpoints[0].resolved_port();
    let mut hosts = Vec::with_capacity(endpoints.len());
    for e in endpoints {
        if e.scheme != *scheme || e.resolved_port() != port {
            return None;
        }
        if e.is_ip() {
            return None;
        }
        let host = normalize(&e.host);
        if !is_valid_hostname(&host) {
            return None;
        }
        hosts.push(host);
    }

    let suffix = common_registrable_suffix(&hosts)?;
    Some(host_with_port(suffix, port, default_port(scheme)))
}

fn host_with_port(host: String, port: u16, standard_port: u16) -> String {
    if port == standard_port {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Longest registrable common suffix shared by all hostnames.
///
/// The suffix must itself be a registrable domain (PSL eTLD+1), of ≥2
/// labels, and not a bare public suffix (`psl::domain_str` returns
/// `None` for public suffixes like `co.uk`).
fn common_registrable_suffix(hosts: &[String]) -> Option<String> {
    let label_sets: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').collect::<Vec<_>>())
        .collect();
    // From the largest candidate (whole "longest suffix") downward.
    let mut k = label_sets.iter().map(|l| l.len()).min().unwrap_or(0);
    while k >= 2 {
        let candidate: Option<String> = {
            let first = &label_sets[0];
            let labels = first[first.len() - k..].to_vec();
            let ok = label_sets.iter().all(|l| l[l.len() - k..] == labels[..]);
            if ok {
                let joined = labels.join(".");
                // The suffix must be a registrable domain itself, with
                // at least 2 labels (checked by `k >= 2`).
                if psl::domain_str(&joined) == Some(joined.as_str()) {
                    Some(joined)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(c) = candidate {
            return Some(c);
        }
        k -= 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Endpoint;

    fn ep(scheme: &str, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_host() {
        assert_eq!(
            derive(&[ep("https", "api.openai.com", None)]).as_deref(),
            Some("api.openai.com")
        );
        assert_eq!(
            derive(&[ep("http", "mock.local", Some(80))]).as_deref(),
            Some("mock.local")
        );
    }

    #[test]
    fn single_hostname_nonstandard_port_derives_host_port() {
        assert_eq!(
            derive(&[ep("https", "api.openai.com", Some(8443))]).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn ip_requires_explicit_alias() {
        assert_eq!(derive(&[ep("http", "127.0.0.1", None)]), None);
        assert_eq!(derive(&[ep("https", "10.0.1.1", None)]), None);
    }

    #[test]
    fn common_suffix_pool_derives_registrable_domain() {
        let pool = [ep("https", "us.vendor.com", None), ep("https", "eu.vendor.com", None)];
        assert_eq!(derive(&pool).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn common_suffix_preserves_nonstandard_port() {
        let pool = [
            ep("https", "us.vendor.com", Some(8443)),
            ep("https", "eu.vendor.com", Some(8443)),
        ];
        assert_eq!(derive(&pool).as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let pool = [ep("https", "foo.co.uk", None), ep("https", "bar.co.uk", None)];
        assert_eq!(derive(&pool), None);
    }

    #[test]
    fn no_common_registrable_suffix_is_not_derivable() {
        let pool = [ep("https", "us.foo.com", None), ep("https", "eu.bar.com", None)];
        assert_eq!(derive(&pool), None);
    }

    #[test]
    fn normalization_lowercases_and_strips_dots() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    }
}
