//! Alias derivation, validation and update transitions.
//!
//! Mirrors `docs/DESIGN.md` §Alias Enforcement Rules: alias behaviour is
//! determined entirely by endpoint type, aliases are not arbitrary labels.

use super::model::{Endpoint, EndpointScheme};

/// Maximum length of a `DNS` hostname.
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a single `DNS` label.
const MAX_LABEL_LEN: usize = 63;
/// Minimum number of labels for a registrable common suffix.
const MIN_SUFFIX_LABELS: usize = 2;

/// The outcome of alias derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// Single hostname on a standard port: `api.openai.com` → `api.openai.com`.
    Hostname(String),
    /// Single hostname on a non-standard port: `api.openai.com:8443`.
    HostPort(String),
    /// Multiple hostnames with a registrable common suffix on a standard port.
    CommonSuffix(String),
    /// Multiple hostnames with a registrable common suffix on a shared
    /// non-standard port: `vendor.com:8443`.
    CommonSuffixPort(String),
    /// Not derivable: an explicit alias was supplied (`IP` pools, heterogeneous
    /// hostnames, bare-public-suffix pools).
    Explicit(String),
}

impl AliasDerivation {
    /// The alias string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Hostname(a) | Self::HostPort(a) | Self::CommonSuffix(a)
            | Self::CommonSuffixPort(a) | Self::Explicit(a) => a,
        }
    }

    /// True when the alias was derived from a multi-host common suffix.
    #[must_use]
    pub fn is_common_suffix(&self) -> bool {
        matches!(self, Self::CommonSuffix(_) | Self::CommonSuffixPort(_))
    }
}

/// Normalizes a hostname or alias: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// Validates a hostname per `RFC 1123`. A trailing dot is tolerated.
///
/// # Errors
///
/// Returns a human-readable reason when the value is not a valid hostname.
pub fn validate_hostname(host: &str) -> Result<(), String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() {
        return Err("hostname is empty".to_owned());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(format!("hostname exceeds {MAX_HOSTNAME_LEN} characters"));
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    for label in host.split('.') {
        validate_label(label)?;
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), String> {
    if label.is_empty() {
        return Err("hostname has an empty label".to_owned());
    }
    if label.len() > MAX_LABEL_LEN {
        return Err("hostname label exceeds 63 characters".to_owned());
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err("hostname label cannot start or end with a hyphen".to_owned());
    }
    if !label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("hostname label contains characters outside [a-zA-Z0-9-]".to_owned());
    }
    Ok(())
}

/// True when `value` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(value: &str) -> bool {
    let stripped = value.trim().trim_start_matches('[').trim_end_matches(']');
    stripped.parse::<std::net::IpAddr>().is_ok()
}

/// Validates a user-supplied alias.
///
/// Aliases are normalized hostnames, optionally carrying a `:port` suffix.
///
/// # Errors
///
/// Returns a human-readable reason when the alias is malformed.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("alias is empty".to_owned());
    }
    if let Some((host, port)) = split_host_port(alias) {
        validate_hostname(host)?;
        if port == 0 {
            return Err("alias port must be between 1 and 65535".to_owned());
        }
    } else {
        validate_hostname(alias)?;
    }
    Ok(())
}

/// Splits `host:port`, tolerating bracketed IPv6 literals.
#[must_use]
pub fn split_host_port(value: &str) -> Option<(&str, u16)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        let port = port.parse::<u16>().ok()?;
        return Some((host, port));
    }
    if value.matches(':').count() > 1 {
        // Bare IPv6 literal without a port.
        return None;
    }
    let (host, port) = value.split_once(':')?;
    let port = port.parse::<u16>().ok()?;
    Some((host, port))
}

/// Validates an `X-OAGW-Target-Host` value: a bare hostname, IPv4 address or
/// IPv6 literal. A port is not accepted.
///
/// # Errors
///
/// Returns a human-readable reason when the value is not a valid target host.
pub fn validate_target_host(value: &str) -> Result<(), String> {
    if value.starts_with('[') {
        let Some(inner) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            return Err("malformed IPv6 literal".to_owned());
        };
        return inner
            .parse::<std::net::Ipv6Addr>()
            .map(|_| ())
            .map_err(|_| "malformed IPv6 literal".to_owned());
    }
    if value.contains(':') {
        return value
            .parse::<std::net::Ipv6Addr>()
            .map(|_| ())
            .map_err(|_| format!("'{value}' must not carry a port"));
    }
    validate_hostname(value)
}

/// Computes the alias implied by a set of endpoints, if one can be derived.
#[must_use]
pub fn derive(endpoints: &[Endpoint]) -> Option<AliasDerivation> {
    if endpoints.is_empty() {
        return None;
    }

    let scheme = endpoints[0].scheme;
    if endpoints.iter().any(|e| e.scheme != scheme) {
        return None;
    }

    if endpoints.len() == 1 {
        let endpoint = &endpoints[0];
        if is_ip_literal(&endpoint.host) {
            return None;
        }
        let host = normalize(&endpoint.host);
        if endpoint.port == scheme.standard_port() {
            return Some(AliasDerivation::Hostname(host));
        }
        return Some(AliasDerivation::HostPort(format!("{host}:{}", endpoint.port)));
    }

    let hosts: Vec<String> = endpoints.iter().map(|e| normalize(&e.host)).collect();
    if hosts.iter().any(|h| is_ip_literal(h)) {
        return None;
    }

    let ports: Vec<u16> = endpoints.iter().map(|e| e.port).collect();
    if ports.iter().any(|p| *p != ports[0]) {
        return None;
    }

    let suffix = common_domain_suffix(&hosts)?;

    if ports[0] == scheme.standard_port() {
        Some(AliasDerivation::CommonSuffix(suffix))
    } else {
        Some(AliasDerivation::CommonSuffixPort(format!(
            "{suffix}:{}",
            ports[0]
        )))
    }
}

/// Longest registrable common domain suffix shared by every host.
///
/// Returns `None` when the hosts share no common suffix, when the shared
/// suffix has fewer than two labels, or when the shared suffix is itself a
/// bare public suffix (for example `co.uk`).
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let labels: Vec<Vec<String>> = hosts
        .iter()
        .map(|h| normalize(h).split('.').map(str::to_owned).collect())
        .collect();

    let shortest = labels.iter().map(Vec::len).min()?;
    let mut common = 0usize;
    for depth in 1..=shortest {
        let expected = &labels[0][labels[0].len() - depth..];
        if labels
            .iter()
            .all(|label| &label[label.len() - depth..] == expected)
        {
            common = depth;
        } else {
            break;
        }
    }

    if common < MIN_SUFFIX_LABELS {
        return None;
    }

    let suffix = labels[0][labels[0].len() - common..].join(".");
    if is_registrable(&suffix) {
        Some(suffix)
    } else {
        None
    }
}

/// True when `candidate` is a registrable domain: it carries at least one
/// label beyond its public suffix.
#[must_use]
pub fn is_registrable(candidate: &str) -> bool {
    let Some(public_suffix) = psl::suffix_str(candidate) else {
        return false;
    };
    candidate.len() > public_suffix.len()
        && candidate.ends_with(public_suffix)
        && candidate.as_bytes().get(candidate.len() - public_suffix.len() - 1) == Some(&b'.')
}

/// Decides the alias for a create, enforcing the endpoint-type rules.
///
/// # Errors
///
/// Returns a `(message, is_alias_conflict)` pair when the supplied alias is
/// not acceptable for these endpoints.
pub fn enforce_create(
    requested: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<(String, AliasDerivation), (String, bool)> {
    let derived = derive(endpoints);
    let normalized = requested.map(normalize).filter(|s| !s.is_empty());

    match derived {
        Some(derivation) => match normalized {
            None => Ok((derivation.as_str().to_owned(), derivation)),
            Some(alias) => {
                validate_alias(&alias).map_err(|e| (e, false))?;
                if alias == derivation.as_str() {
                    Ok((alias, derivation))
                } else {
                    Err((
                        format!(
                            "alias '{alias}' does not match the derived alias '{}'; hostname-based endpoints always auto-derive their alias",
                            derivation.as_str()
                        ),
                        false,
                    ))
                }
            }
        },
        None => match normalized {
            None => Err((
                "alias is required for IP-based or non-derivable endpoints".to_owned(),
                false,
            )),
            Some(alias) => {
                validate_alias(&alias).map_err(|e| (e, false))?;
                Ok((alias.clone(), AliasDerivation::Explicit(alias)))
            }
        },
    }
}

/// Enforces alias immutability across an endpoint change.
///
/// # Errors
///
/// Returns a message when the transition would change the alias.
pub fn enforce_update(
    existing_alias: &str,
    requested: Option<&str>,
    existing_endpoints: &[Endpoint],
    new_endpoints: &[Endpoint],
) -> Result<(), String> {
    if let Some(alias) = requested.map(normalize).filter(|s| !s.is_empty())
        && alias != existing_alias
    {
        return Err(format!("alias is immutable once set; refusing to rename '{existing_alias}' to '{alias}'"));
    }

    if existing_endpoints == new_endpoints {
        return Ok(());
    }

    // An alias that the existing endpoints cannot derive is operator-assigned,
    // so an endpoint change keeps it as long as the new set does not imply a
    // different one.
    let existing_derivable = derive(existing_endpoints).is_some();
    match (existing_derivable, derive(new_endpoints)) {
        (true, Some(derivation)) if derivation.as_str() == existing_alias => Ok(()),
        (true, Some(derivation)) => Err(format!(
            "endpoint change would change the derived alias from '{existing_alias}' to '{}'; delete and re-create the upstream instead",
            derivation.as_str()
        )),
        (true, None) => Err(
            "endpoint change would make the alias non-derivable; delete and re-create the upstream instead"
                .to_owned(),
        ),
        (false, Some(derivation)) if derivation.as_str() == existing_alias => Ok(()),
        (false, Some(_)) => Err(format!(
            "endpoint change would change the derived alias from '{existing_alias}'; delete and re-create the upstream instead"
        )),
        // Non-derivable -> non-derivable: the existing alias is retained.
        (false, None) => Ok(()),
    }
}

/// True when `scheme` is `http`-derived and a plaintext connection would be
/// needed. Kept separate from scheme validity: the schema admits only
/// `https|wss|wt|grpc`, and plaintext dialling is a gear configuration matter.
#[must_use]
pub fn requires_plaintext(scheme: EndpointScheme, allow_http_upstream: bool) -> bool {
    allow_http_upstream && !scheme.is_tls()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ep(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
        Endpoint { scheme, host: host.to_owned(), port }
    }

    #[test]
    fn normalizes_case_and_trailing_dot() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn derives_single_hostname() {
        let d = derive(&[ep(EndpointScheme::Https, "api.openai.com", 443)]).unwrap();
        assert_eq!(d.as_str(), "api.openai.com");
        assert!(!d.is_common_suffix());
    }

    #[test]
    fn derives_single_hostname_with_port() {
        let d = derive(&[ep(EndpointScheme::Https, "api.openai.com", 8443)]).unwrap();
        assert_eq!(d.as_str(), "api.openai.com:8443");
    }

    #[test]
    fn derives_common_suffix() {
        let d = derive(&[
            ep(EndpointScheme::Https, "us.vendor.com", 443),
            ep(EndpointScheme::Https, "eu.vendor.com", 443),
        ])
        .unwrap();
        assert_eq!(d.as_str(), "vendor.com");
        assert!(d.is_common_suffix());
    }

    #[test]
    fn derives_common_suffix_with_port() {
        let d = derive(&[
            ep(EndpointScheme::Https, "us.vendor.com", 8443),
            ep(EndpointScheme::Https, "eu.vendor.com", 8443),
        ])
        .unwrap();
        assert_eq!(d.as_str(), "vendor.com:8443");
        assert!(d.is_common_suffix());
    }

    #[test]
    fn rejects_bare_public_suffix() {
        assert!(
            derive(&[
                ep(EndpointScheme::Https, "foo.co.uk", 443),
                ep(EndpointScheme::Https, "bar.co.uk", 443),
            ])
            .is_none()
        );
    }

    #[test]
    fn rejects_heterogeneous_hosts() {
        assert!(
            derive(&[
                ep(EndpointScheme::Https, "us.foo.com", 443),
                ep(EndpointScheme::Https, "eu.bar.com", 443),
            ])
            .is_none()
        );
    }

    #[test]
    fn rejects_ip_endpoints() {
        assert!(derive(&[ep(EndpointScheme::Https, "10.0.1.1", 443)]).is_none());
        assert!(
            derive(&[
                ep(EndpointScheme::Https, "10.0.1.1", 443),
                ep(EndpointScheme::Https, "10.0.1.2", 443),
            ])
            .is_none()
        );
    }

    #[test]
    fn rejects_unequal_ports() {
        assert!(
            derive(&[
                ep(EndpointScheme::Https, "us.vendor.com", 443),
                ep(EndpointScheme::Https, "eu.vendor.com", 8443),
            ])
            .is_none()
        );
    }

    #[test]
    fn hostname_validation() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("a-b.c-d.example").is_ok());
        assert!(validate_hostname("10.0.0.1").is_ok());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("bad_label.example.com").is_err());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(60))).is_ok());
    }

    #[test]
    fn target_host_validation() {
        assert!(validate_target_host("us.vendor.com").is_ok());
        assert!(validate_target_host("10.0.0.1").is_ok());
        assert!(validate_target_host("::1").is_ok());
        assert!(validate_target_host("[::1]").is_ok());
        assert!(validate_target_host("us.vendor.com:8443").is_err());
        assert!(validate_target_host("").is_err());
    }

    #[test]
    fn alias_validation_accepts_host_and_host_port() {
        assert!(validate_alias("vendor.com").is_ok());
        assert!(validate_alias("vendor.com:8443").is_ok());
        assert!(validate_alias("my-service").is_ok());
        assert!(validate_alias("my-service:0").is_err());
    }

    #[test]
    fn create_requires_explicit_alias_for_ips() {
        let eps = [ep(EndpointScheme::Https, "10.0.1.1", 443)];
        assert!(enforce_create(None, &eps).is_err());
        let (alias, _) = enforce_create(Some("my-service"), &eps).unwrap();
        assert_eq!(alias, "my-service");
    }

    #[test]
    fn create_rejects_alias_override_for_hostnames() {
        let eps = [ep(EndpointScheme::Https, "api.openai.com", 443)];
        assert!(enforce_create(Some("other.name"), &eps).is_err());
        let (alias, _) = enforce_create(Some("api.openai.com"), &eps).unwrap();
        assert_eq!(alias, "api.openai.com");
    }

    #[test]
    fn update_rejects_alias_change() {
        let eps = [ep(EndpointScheme::Https, "10.0.1.1", 443)];
        assert!(enforce_update("my-service", Some("other"), &eps, &eps).is_err());
        assert!(enforce_update("my-service", Some("my-service"), &eps, &eps).is_ok());
    }

    #[test]
    fn update_rejects_derived_alias_change() {
        let old = [ep(EndpointScheme::Https, "api.openai.com", 443)];
        let new = [ep(EndpointScheme::Https, "api.other.com", 443)];
        assert!(enforce_update("api.openai.com", None, &old, &new).is_err());
    }

    #[test]
    fn update_allows_alias_preserving_endpoint_change() {
        let old = [ep(EndpointScheme::Https, "us.vendor.com", 443)];
        let new = [
            ep(EndpointScheme::Https, "us.vendor.com", 443),
            ep(EndpointScheme::Https, "eu.vendor.com", 443),
        ];
        assert!(enforce_update("vendor.com", None, &old, &new).is_ok());
    }

    #[test]
    fn update_allows_ip_pool_growth() {
        let old = [ep(EndpointScheme::Https, "10.0.1.1", 443)];
        let new = [
            ep(EndpointScheme::Https, "10.0.1.1", 443),
            ep(EndpointScheme::Https, "10.0.1.2", 443),
        ];
        assert!(enforce_update("my-service", None, &old, &new).is_ok());
    }

    #[test]
    fn plaintext_flag_is_orthogonal_to_scheme_validity() {
        // The schema admits only TLS-bearing schemes, so a `https` endpoint
        // never dials plaintext, whatever the flag says.
        assert!(!requires_plaintext(EndpointScheme::Https, true));
        assert!(!requires_plaintext(EndpointScheme::Https, false));
    }
}
