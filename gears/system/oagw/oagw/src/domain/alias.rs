//! Alias derivation, normalization and update transitions.
//!
//! Implements the derivation table of `DESIGN.md` §3.3 / PRD §5.5:
//!
//! | Endpoint type | Rule |
//! |---|---|
//! | hostname + standard port | auto-derived (hostname) |
//! | hostname + non-standard port | auto-derived (host:port) |
//! | multiple hostnames, registrable common suffix | auto-derived via PSL |
//! | multiple hostnames, common suffix is a bare public suffix | explicit alias required |
//! | multiple hostnames, no common suffix | explicit alias required |
//! | IP addresses | explicit alias required |

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::Endpoint;

/// Normalizes an alias: ASCII lowercase, trailing dots stripped.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] when the alias does not match the
/// wire pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` after normalization.
pub fn normalize_alias(value: &str) -> Result<String, DomainError> {
    let lowered = value.to_ascii_lowercase();
    let trimmed = lowered.trim_end_matches('.');
    let valid = !trimmed.is_empty()
        && trimmed.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == ':' || c == '-'
        })
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && trimmed
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !valid {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            format!("invalid alias `{value}`: expected lowercase host-like identifier"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Whether `host` is an IP address (v4 or v6).
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok() || host.parse::<std::net::Ipv6Addr>().is_ok()
}

/// The registrable domain of `host` according to the public suffix list.
///
/// Returns `None` when the host is not a registrable domain (bare public
/// suffix such as `co.uk`, single-label hosts, IP addresses).
#[must_use]
pub fn registrable_domain(host: &str) -> Option<String> {
    let lower = host.to_ascii_lowercase();
    psl::domain_str(&lower).map(str::to_owned)
}

/// Derives the alias of an upstream from its endpoint pool.
///
/// Returns `None` when the endpoints cannot be summarized (IP addresses,
/// heterogeneous pools without a registrable common suffix).
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints.is_empty() {
        return None;
    }
    // A mixed pool (different hosts and ports) is not derivable.
    if endpoints.len() == 1 {
        let endpoint = first;
        if is_ip_address(&endpoint.host) {
            return None;
        }
        return Some(host_alias(
            &endpoint.host,
            endpoint.effective_port(),
            endpoint.scheme,
        ));
    }
    let non_standard = endpoints.iter().any(|e| !e.is_standard_port());
    if non_standard {
        // All endpoints must share the port for a suffix derivation.
        let port = first.effective_port();
        if endpoints.iter().any(|e| e.effective_port() != port) {
            return None;
        }
    }
    let scheme = first.scheme;
    if endpoints
        .iter()
        .any(|e| e.scheme != scheme || is_ip_address(&e.host))
    {
        return None;
    }
    let hosts: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
    let suffix = common_domain_suffix(&hosts)?;
    let port = first.effective_port();
    if non_standard {
        Some(format!("{suffix}:{port}"))
    } else {
        Some(suffix)
    }
}

/// Alias of a single host, keeping non-standard ports.
fn host_alias(host: &str, port: u16, scheme: crate::domain::model::EndpointScheme) -> String {
    if scheme.is_standard_port(port) {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

/// The registrable common suffix of `hosts`, when every host shares one and it
/// is not itself a bare public suffix.
#[must_use]
pub fn common_domain_suffix(hosts: &[&str]) -> Option<String> {
    if hosts.len() < 2 {
        return None;
    }
    let domains: Vec<String> = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
    let first = domains.first()?.clone();
    let labels: Vec<&str> = first.split('.').collect();
    // Walk candidate suffixes from the shortest registrable form upward.
    for skip in 1..labels.len() {
        let candidate = labels[skip..].join(".");
        if candidate.is_empty() {
            continue;
        }
        if domains
            .iter()
            .all(|d| d.as_str() == candidate || d.ends_with(&format!(".{candidate}")))
        {
            let registrable = registrable_domain(&candidate)?;
            if registrable == candidate {
                return Some(candidate);
            }
            return None;
        }
    }
    None
}

/// Alias state of an upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// The alias is derivable from the endpoints, with this value.
    Derivable(String),
    /// An explicit alias is required.
    Explicit,
}

/// Classifies the alias requirement of an upstream configuration.
#[must_use]
pub fn alias_requirement(endpoints: &[Endpoint]) -> AliasDerivation {
    match compute_derived_alias(endpoints) {
        Some(alias) => AliasDerivation::Derivable(alias),
        None => AliasDerivation::Explicit,
    }
}

/// Resolves the alias to store at create time.
///
/// * derivable endpoints + no alias → derived alias
/// * derivable endpoints + alias equal to the derived one → accepted (idempotent)
/// * derivable endpoints + different alias → rejected
/// * non-derivable endpoints + no alias → rejected
/// * non-derivable endpoints + alias → accepted as-is
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for the rejected combinations.
pub fn enforce_alias_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    let normalized = provided.map(normalize_alias).transpose()?;
    match alias_requirement(endpoints) {
        AliasDerivation::Derivable(derived) => match normalized {
            None => Ok(derived),
            Some(alias) if alias == derived => Ok(alias),
            Some(alias) => Err(DomainError::new(
                ErrorKind::ValidationError,
                format!(
                    "alias `{alias}` does not match the derived alias `{derived}` for hostname endpoints"
                ),
            )),
        },
        AliasDerivation::Explicit => normalized.ok_or_else(|| {
            DomainError::new(
                ErrorKind::ValidationError,
                "an explicit alias is required for IP-based or non-derivable endpoints",
            )
        }),
    }
}

/// Resolves the alias on update.
///
/// Transitions (DESIGN.md §3.3):
/// * derivable → derivable: allowed only when the recomputed alias equals the
///   existing one (or when no alias is provided, keeping the existing alias).
/// * derivable → non-derivable: always rejected.
/// * non-derivable → non-derivable: retains the existing alias; a provided
///   alias must match it.
/// * non-derivable → derivable: allowed only when the derived alias equals the
///   existing one.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for rejected transitions.
pub fn enforce_alias_update(
    endpoints: &[Endpoint],
    existing: &str,
    provided: Option<&str>,
) -> Result<String, DomainError> {
    let normalized = provided.map(normalize_alias).transpose()?;
    match alias_requirement(endpoints) {
        AliasDerivation::Derivable(derived) => {
            if normalized
                .as_ref()
                .is_some_and(|alias| alias.as_str() != derived)
            {
                let alias = normalized.unwrap_or_default();
                return Err(DomainError::new(
                    ErrorKind::ValidationError,
                    format!(
                        "alias `{alias}` does not match the derived alias `{derived}` for hostname endpoints"
                    ),
                ));
            }
            if derived != existing {
                return Err(DomainError::new(
                    ErrorKind::ValidationError,
                    format!(
                        "changing alias from `{existing}` to `{derived}` is not allowed once assigned"
                    ),
                ));
            }
            Ok(existing.to_owned())
        }
        AliasDerivation::Explicit => match normalized {
            None => Ok(existing.to_owned()),
            Some(alias) if alias == existing => Ok(existing.to_owned()),
            Some(alias) => Err(DomainError::new(
                ErrorKind::ValidationError,
                format!("alias of non-derivable endpoints is immutable, `{alias}` != `{existing}`"),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{EndpointScheme, ServerConfig};

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn derives_single_host_aliases_per_the_table() {
        let https = endpoint(EndpointScheme::Https, "api.openai.com", 443);
        assert_eq!(
            compute_derived_alias(&[https]),
            Some("api.openai.com".to_owned())
        );
        let non_standard = endpoint(EndpointScheme::Https, "api.openai.com", 8443);
        assert_eq!(
            compute_derived_alias(&[non_standard]),
            Some("api.openai.com:8443".to_owned())
        );
        let http = endpoint(EndpointScheme::Http, "internal.example.com", 80);
        assert_eq!(
            compute_derived_alias(&[http]),
            Some("internal.example.com".to_owned())
        );
        let http_port = endpoint(EndpointScheme::Http, "internal.example.com", 8080);
        assert_eq!(
            compute_derived_alias(&[http_port]),
            Some("internal.example.com:8080".to_owned())
        );
    }

    #[test]
    fn derives_multi_host_suffix_aliases() {
        let pool = vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(compute_derived_alias(&pool), Some("vendor.com".to_owned()));
        let with_ports = vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 8443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&with_ports),
            Some("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn requires_explicit_alias_for_the_undervivable_rows() {
        let bare_public_suffix = vec![
            endpoint(EndpointScheme::Https, "foo.co.uk", 443),
            endpoint(EndpointScheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(compute_derived_alias(&bare_public_suffix), None);

        let heterogeneous = vec![
            endpoint(EndpointScheme::Https, "us.foo.com", 443),
            endpoint(EndpointScheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(compute_derived_alias(&heterogeneous), None);

        let ips = vec![
            endpoint(EndpointScheme::Http, "10.0.1.1", 8080),
            endpoint(EndpointScheme::Http, "10.0.1.2", 8080),
        ];
        assert_eq!(compute_derived_alias(&ips), None);

        let single_ip = vec![endpoint(EndpointScheme::Http, "10.0.1.1", 8080)];
        assert_eq!(compute_derived_alias(&single_ip), None);

        let ipv6 = vec![endpoint(EndpointScheme::Https, "2001:db8::1", 443)];
        assert_eq!(compute_derived_alias(&ipv6), None);

        let mixed_ports = vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 8443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(compute_derived_alias(&mixed_ports), None);
    }

    #[test]
    fn normalizes_and_validates_aliases() {
        assert_eq!(
            normalize_alias("Api.OpenAI.COM.").ok(),
            Some("api.openai.com".to_owned())
        );
        assert_eq!(
            normalize_alias("API:8443").ok(),
            Some("api:8443".to_owned())
        );
        assert!(normalize_alias("-bad-").is_err());
        assert!(normalize_alias("").is_err());
        assert!(normalize_alias("with space").is_err());
    }

    #[test]
    fn create_enforces_the_alias_rules() {
        let pool = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert_eq!(
            enforce_alias_create(&pool, None).ok(),
            Some("api.openai.com".to_owned())
        );
        assert_eq!(
            enforce_alias_create(&pool, Some("api.openai.com")).ok(),
            Some("api.openai.com".to_owned())
        );
        assert_eq!(
            enforce_alias_create(&pool, Some("API.OPENAI.COM.")).ok(),
            Some("api.openai.com".to_owned())
        );
        assert!(enforce_alias_create(&pool, Some("other")).is_err());

        let ips = vec![endpoint(EndpointScheme::Http, "10.0.1.1", 8080)];
        assert!(enforce_alias_create(&ips, None).is_err());
        assert_eq!(
            enforce_alias_create(&ips, Some("internal-svc")).ok(),
            Some("internal-svc".to_owned())
        );
    }

    #[test]
    fn update_follows_the_transition_table() {
        let derivable = vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)];
        assert_eq!(
            enforce_alias_update(&derivable, "api.openai.com", None).ok(),
            Some("api.openai.com".to_owned())
        );
        assert!(enforce_alias_update(&derivable, "api.openai.com", Some("other")).is_err());
        let different = vec![endpoint(EndpointScheme::Https, "api.other.com", 443)];
        assert!(enforce_alias_update(&different, "api.openai.com", None).is_err());

        let non_derivable = vec![endpoint(EndpointScheme::Http, "10.0.1.1", 8080)];
        assert_eq!(
            enforce_alias_update(&non_derivable, "internal-svc", None).ok(),
            Some("internal-svc".to_owned())
        );
        assert_eq!(
            enforce_alias_update(&non_derivable, "internal-svc", Some("internal-svc")).ok(),
            Some("internal-svc".to_owned())
        );
        assert!(enforce_alias_update(&non_derivable, "internal-svc", Some("renamed")).is_err());
    }

    #[test]
    fn server_config_holds_the_pool() {
        let spec = ServerConfig {
            endpoints: vec![endpoint(EndpointScheme::Https, "a.example.com", 443)],
        };
        assert_eq!(spec.endpoints.len(), 1);
    }
}
