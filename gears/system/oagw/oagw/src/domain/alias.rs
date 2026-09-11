//! Alias derivation, normalisation, host validation and the update matrix.
//!
//! The rules are exactly the "Alias Enforcement Rules" table of
//! `docs/DESIGN.md`: aliases are not arbitrary labels, they are derived from
//! the endpoints and are immutable once set.

use crate::domain::dto::Endpoint;

/// Standard ports omitted from a derived alias.
const HTTP_STANDARD_PORT: u16 = 80;
/// Standard TLS/gRPC port omitted from a derived alias.
const TLS_STANDARD_PORT: u16 = 443;

/// Lower-cases the alias and strips trailing dots.
pub fn normalise_alias(alias: &str) -> String {
    let mut s = alias.trim().to_ascii_lowercase();
    while s.ends_with('.') {
        s.pop();
    }
    s
}

/// Lower-cases a host and strips a single trailing FQDN dot.
pub fn normalise_host(host: &str) -> String {
    normalise_alias(host)
}

/// Whether `host` is an IPv4 or IPv6 literal.
pub fn is_ip_literal(host: &str) -> bool {
    let candidate = host.trim_end_matches('.');
    if candidate.is_empty() {
        return false;
    }
    if candidate.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    // IPv6 literals are written either bare or in `[..]` form.
    let bare = candidate.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::Ipv6Addr>().is_ok()
}

/// RFC 1123 hostname validation.
///
/// Max 253 characters total; each label 1–63 characters; labels contain only
/// ASCII alphanumeric and hyphen; labels may not start or end with a hyphen.
/// A trailing dot (FQDN notation) is tolerated and stripped.
pub fn is_valid_hostname(host: &str) -> bool {
    let host = normalise_host(host);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if is_ip_literal(&host) {
        return true;
    }
    host.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
        label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Validates an alias against the documented alias pattern:
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` after normalisation.
pub fn is_valid_alias(alias: &str) -> bool {
    let alias = normalise_alias(alias);
    if alias.is_empty() {
        return false;
    }
    let bytes = alias.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b':' || *b == b'-')
}

/// The `host[:port]` form of an endpoint, with the standard port omitted.
fn endpoint_alias_part(endpoint: &Endpoint) -> Option<String> {
    let host = normalise_host(&endpoint.host);
    if !is_valid_hostname(&host) {
        return None;
    }
    let port = endpoint.effective_port();
    let standard = match endpoint.scheme {
        crate::domain::dto::EndpointScheme::Http => HTTP_STANDARD_PORT,
        _ => TLS_STANDARD_PORT,
    };
    if port == standard || port == 0 {
        Some(host)
    } else {
        Some(format!("{host}:{port}"))
    }
}

/// Longest common suffix of a set of `host[:port]` strings.
fn common_suffix(parts: &[String]) -> Option<String> {
    let mut iter = parts.iter();
    let first = iter.next()?;
    let mut suffix: Vec<&str> = first.split('.').collect();
    for part in iter {
        let labels: Vec<&str> = part.split('.').collect();
        let keep = suffix.len().min(labels.len());
        let mut shared = 0;
        while shared < keep
            && suffix[suffix.len() - 1 - shared] == labels[labels.len() - 1 - shared]
        {
            shared += 1;
        }
        if shared == 0 {
            return None;
        }
        suffix = suffix[suffix.len() - shared..].to_vec();
    }
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.join("."))
    }
}

/// Whether a dotted name is a registrable domain and not a bare public suffix.
///
/// A registrable domain needs at least two labels and must not itself be a
/// public suffix (`co.uk` is a public suffix; `vendor.com` is registrable).
fn is_registrable(name: &str) -> bool {
    if name.split('.').count() < 2 {
        return false;
    }
    match psl::suffix_str(name) {
        // The name is itself a public suffix (e.g. `co.uk`) → not registrable.
        Some(public_suffix) => public_suffix != name,
        None => false,
    }
}

/// The alias an upstream's endpoints derive, or `None` when derivation is
/// impossible (IP endpoints, heterogeneous hostnames, or a bare public
/// suffix as the only common suffix).
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let mut parts: Vec<String> = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        parts.push(endpoint_alias_part(endpoint)?);
    }
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 1 {
        return Some(parts.remove(0));
    }

    // All parts must be hostname-shaped (no IP literals in a derivable pool).
    for part in &parts {
        let host = part.split(':').next().unwrap_or(part);
        if is_ip_literal(host) {
            return None;
        }
    }

    // A pool must agree on its port part.
    let ports: Vec<Option<&str>> = parts
        .iter()
        .map(|p| p.split_once(':').map(|(_, port)| port))
        .collect();
    if ports.iter().any(|p| *p != ports[0]) {
        return None;
    }
    let port_suffix = ports[0];

    let hosts: Vec<String> = parts
        .iter()
        .map(|p| p.split(':').next().unwrap_or(p).to_string())
        .collect();
    let suffix = common_suffix(&hosts)?;
    if !is_registrable(&suffix) {
        return None;
    }
    Some(match port_suffix {
        Some(port) => format!("{suffix}:{port}"),
        None => suffix,
    })
}

/// Why an alias is not derivable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivationOutcome {
    /// Derivation produced an alias.
    Derived(String),
    /// Endpoints are IP literals: an explicit alias is required.
    IpEndpoints,
    /// Hostname pool with no registrable common suffix: explicit alias required.
    NotDerivable,
    /// No endpoints at all.
    NoEndpoints,
}

impl DerivationOutcome {
    /// Whether the outcome is a derivation.
    pub fn is_derived(&self) -> bool {
        matches!(self, DerivationOutcome::Derived(_))
    }

    /// The derived alias, when there is one.
    pub fn alias(&self) -> Option<&str> {
        match self {
            DerivationOutcome::Derived(a) => Some(a),
            _ => None,
        }
    }
}

/// Classifies an endpoint set into a derivation outcome.
pub fn derive_alias(endpoints: &[Endpoint]) -> DerivationOutcome {
    if endpoints.is_empty() {
        return DerivationOutcome::NoEndpoints;
    }
    // Only a wholly IP-based pool is "IP endpoints"; a pool mixing literals
    // and hostnames is simply not derivable, like any heterogeneous pool.
    if endpoints.iter().all(|e| is_ip_literal(&normalise_host(&e.host))) {
        return DerivationOutcome::IpEndpoints;
    }
    match compute_derived_alias(endpoints) {
        Some(alias) => DerivationOutcome::Derived(alias),
        None => DerivationOutcome::NotDerivable,
    }
}

use DerivationOutcome::{Derived, IpEndpoints, NotDerivable};

/// The update-immutability matrix of `docs/DESIGN.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasUpdateVerdict {
    /// The existing alias is retained.
    Keep,
    /// The existing alias is retained and equals the derived one.
    KeepDerived,
    /// The request is rejected with a validation error.
    Reject(String),
}

impl AliasUpdateVerdict {
    /// Whether the update may proceed.
    pub fn is_allowed(&self) -> bool {
        !matches!(self, AliasUpdateVerdict::Reject(_))
    }
}

/// Enforces the alias update matrix.
///
/// The existing state's derivability is unknown here, so it is assumed
/// non-derivable. Prefer [`enforce_alias_update_with`], which knows the
/// endpoints being replaced and can therefore tell a hostname → IP transition
/// from an IP → IP one.
pub fn enforce_alias_update(
    existing_alias: &str,
    new_endpoints: &[Endpoint],
    new_alias: Option<&str>,
) -> AliasUpdateVerdict {
    enforce_alias_update_with(existing_alias, &[], new_endpoints, new_alias)
}

/// Enforces the alias update matrix of `docs/DESIGN.md`.
///
/// * `existing_alias` — the alias already stored (already normalised).
/// * `existing_endpoints` — the endpoint set being replaced.
/// * `new_endpoints` — the endpoint set after the update.
/// * `new_alias` — the alias the request carries, when it carries one.
pub fn enforce_alias_update_with(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    new_endpoints: &[Endpoint],
    new_alias: Option<&str>,
) -> AliasUpdateVerdict {
    // Derivable → Derivable and Non-derivable → Derivable are both allowed
    // only when the recomputed derivation equals the alias already stored.
    if let Derived(derived) = derive_alias(new_endpoints) {
        if derived == existing_alias {
            // The exact derived value is tolerated for idempotency; any other
            // alias is an override, which the endpoints do not permit.
            if let Some(provided) = new_alias {
                let provided = normalise_alias(provided);
                if provided != existing_alias {
                    return AliasUpdateVerdict::Reject(format!(
                        "alias is derived from the endpoints: `{provided}` does not match the derived value `{derived}`"
                    ));
                }
            }
            return AliasUpdateVerdict::KeepDerived;
        }
        return AliasUpdateVerdict::Reject(format!(
            "alias is immutable: endpoints would derive `{derived}` but the upstream is bound to `{existing_alias}`; delete and re-create the upstream"
        ));
    }

    // Derivable → Non-derivable: rejected always, alias override or not.
    if derive_alias(existing_endpoints).is_derived() {
        return AliasUpdateVerdict::Reject(format!(
            "alias is immutable: `{existing_alias}` is derived from hostname endpoints, which the update would replace with non-derivable ones; delete and re-create the upstream"
        ));
    }

    // Non-derivable → Non-derivable: the existing alias is retained.
    match new_alias {
        Some(provided) => {
            let provided = normalise_alias(provided);
            if provided == existing_alias {
                AliasUpdateVerdict::Keep
            } else {
                AliasUpdateVerdict::Reject(format!(
                    "alias is immutable: `{existing_alias}` cannot be changed to `{provided}`"
                ))
            }
        }
        None => AliasUpdateVerdict::Keep,
    }
}

/// Enforces the alias of a *new* upstream.
///
/// Returns the alias to store. `provided` is the alias the request carried.
pub fn enforce_alias_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, String> {
    match derive_alias(endpoints) {
        DerivationOutcome::NoEndpoints => Err("at least one endpoint is required".to_string()),
        IpEndpoints | NotDerivable => match provided {
            Some(alias) if !alias.trim().is_empty() => {
                let alias = normalise_alias(alias);
                if !is_valid_alias(&alias) {
                    Err(format!("alias `{alias}` is not a valid routing identifier"))
                } else {
                    Ok(alias)
                }
            }
            _ => Err("an explicit alias is required for IP-based or non-derivable endpoints"
                .to_string()),
        },
        Derived(derived) => match provided {
            None => Ok(derived),
            Some(alias) => {
                let alias = normalise_alias(alias);
                if alias == derived {
                    Ok(derived)
                } else {
                    Err(format!(
                        "alias is derived from the endpoints: `{alias}` does not match the derived value `{derived}`"
                    ))
                }
            }
        },
    }
}

/// Validates an endpoint host per RFC 1123 (or as an IP literal).
pub fn validate_endpoint_host(endpoint: &Endpoint) -> Result<(), String> {
    let host = normalise_host(&endpoint.host);
    if host.is_empty() {
        return Err("endpoint host is required".to_string());
    }
    if !is_valid_hostname(&host) {
        return Err(format!("`{}` is not a valid hostname or IP address", endpoint.host));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Endpoint, EndpointScheme};

    fn ep(host: &str, port: u16) -> Endpoint {
        Endpoint { scheme: EndpointScheme::Https, host: host.to_string(), port }
    }

    fn http_ep(host: &str, port: u16) -> Endpoint {
        Endpoint { scheme: EndpointScheme::Http, host: host.to_string(), port }
    }

    #[test]
    fn normalisation_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalise_alias("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalise_host("Vendor.COM."), "vendor.com");
        assert_eq!(normalise_alias("..."), "");
    }

    #[test]
    fn single_hostname_with_standard_port_derives_the_hostname() {
        assert_eq!(
            compute_derived_alias(&[ep("api.openai.com", 443)]).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn single_hostname_with_non_standard_port_derives_the_port_form() {
        assert_eq!(
            compute_derived_alias(&[ep("api.openai.com", 8443)]).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn http_endpoints_use_port_80_as_the_standard_port() {
        assert_eq!(
            compute_derived_alias(&[http_ep("api.openai.com", 80)]).as_deref(),
            Some("api.openai.com")
        );
        assert_eq!(
            compute_derived_alias(&[http_ep("api.openai.com", 8080)]).as_deref(),
            Some("api.openai.com:8080")
        );
    }

    #[test]
    fn common_registrable_suffix_is_derived() {
        assert_eq!(
            compute_derived_alias(&[ep("us.vendor.com", 443), ep("eu.vendor.com", 443)])
                .as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn common_suffix_with_non_standard_port_keeps_the_port() {
        assert_eq!(
            compute_derived_alias(&[ep("us.vendor.com", 8443), ep("eu.vendor.com", 8443)])
                .as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn a_bare_public_suffix_is_not_derivable() {
        assert_eq!(
            derive_alias(&[ep("foo.co.uk", 443), ep("bar.co.uk", 443)]),
            DerivationOutcome::NotDerivable
        );
    }

    #[test]
    fn heterogeneous_hostnames_are_not_derivable() {
        assert_eq!(
            derive_alias(&[ep("us.foo.com", 443), ep("eu.bar.com", 443)]),
            DerivationOutcome::NotDerivable
        );
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        assert_eq!(
            derive_alias(&[ep("10.0.1.1", 443), ep("10.0.1.2", 443)]),
            DerivationOutcome::IpEndpoints
        );
        assert_eq!(derive_alias(&[ep("10.0.1.1", 443)]), DerivationOutcome::IpEndpoints);
    }

    #[test]
    fn pools_disagreeing_on_port_are_not_derivable() {
        assert_eq!(
            derive_alias(&[ep("us.vendor.com", 443), ep("eu.vendor.com", 8443)]),
            DerivationOutcome::NotDerivable
        );
    }

    #[test]
    fn case_and_trailing_dots_are_normalised_before_deriving() {
        assert_eq!(
            compute_derived_alias(&[ep("US.Vendor.COM.", 443), ep("eu.VENDOR.com.", 443)])
                .as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn rfc1123_host_validation() {
        assert!(is_valid_hostname("api.openai.com"));
        assert!(is_valid_hostname("api.openai.com."));
        assert!(is_valid_hostname("a-b.c-d.example"));
        assert!(is_valid_hostname("10.0.1.1"));
        assert!(!is_valid_hostname("-bad.example.com"));
        assert!(!is_valid_hostname("bad-.example.com"));
        assert!(!is_valid_hostname("bad_.example.com"));
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname(&"a".repeat(254)));
    }

    #[test]
    fn alias_validation_follows_the_documented_pattern() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("my-service"));
        assert!(is_valid_alias("vendor.com:8443"));
        assert!(!is_valid_alias("-leading"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("with space"));
        // A trailing dot is stripped by normalisation, not a reason to refuse.
        assert!(is_valid_alias("trailing."));
        assert_eq!(normalise_alias("trailing."), "trailing");
    }

    #[test]
    fn create_requires_an_explicit_alias_for_ip_endpoints() {
        let err = enforce_alias_create(&[ep("10.0.1.1", 443)], None);
        assert!(err.is_err());
        assert_eq!(
            enforce_alias_create(&[ep("10.0.1.1", 443)], Some("my-service")).unwrap(),
            "my-service".to_string()
        );
    }

    #[test]
    fn create_rejects_an_alias_that_differs_from_the_derived_value() {
        let err = enforce_alias_create(&[ep("api.openai.com", 443)], Some("other"));
        assert!(err.is_err());
        assert_eq!(
            enforce_alias_create(&[ep("api.openai.com", 443)], Some("api.openai.com")).unwrap(),
            "api.openai.com".to_string(),
            "the exact derived value is tolerated for idempotency"
        );
    }

    #[test]
    fn update_matrix_derivable_to_derivable() {
        let same = [ep("api.openai.com", 443)];
        let changed = [ep("api.other.com", 443)];

        assert!(enforce_alias_update_with("api.openai.com", &same, &same, None).is_allowed());
        assert!(
            enforce_alias_update_with("api.openai.com", &same, &same, Some("api.openai.com"))
                .is_allowed()
        );
        assert!(!enforce_alias_update_with("api.openai.com", &same, &changed, None).is_allowed());
    }

    #[test]
    fn update_matrix_derivable_to_non_derivable_is_always_rejected() {
        let hosts = [ep("api.openai.com", 443)];
        let ips = [ep("10.0.1.1", 443)];
        assert!(!enforce_alias_update_with("api.openai.com", &hosts, &ips, Some("my-service"))
            .is_allowed());
        assert!(!enforce_alias_update_with("api.openai.com", &hosts, &ips, None).is_allowed());
        assert!(
            !enforce_alias_update_with("api.openai.com", &hosts, &ips, Some("api.openai.com"))
                .is_allowed()
        );
    }

    #[test]
    fn update_matrix_non_derivable_to_non_derivable() {
        let same = [ep("10.0.1.1", 443)];
        assert!(enforce_alias_update_with("my-service", &same, &same, None).is_allowed());
        assert!(enforce_alias_update_with("my-service", &same, &same, Some("my-service"))
            .is_allowed());
        assert!(!enforce_alias_update_with("my-service", &same, &same, Some("renamed"))
            .is_allowed());
    }

    #[test]
    fn update_matrix_non_derivable_to_derivable() {
        let ips = [ep("10.0.1.1", 443)];
        let host = [ep("api.openai.com", 443)];
        assert!(enforce_alias_update_with("api.openai.com", &ips, &host, None).is_allowed());
        assert!(!enforce_alias_update_with("my-service", &ips, &host, None).is_allowed());
    }

    #[test]
    fn validate_endpoint_host_reports_bad_values() {
        assert!(validate_endpoint_host(&ep("api.openai.com", 443)).is_ok());
        assert!(validate_endpoint_host(&ep("10.0.1.1", 443)).is_ok());
        assert!(validate_endpoint_host(&ep("", 443)).is_err());
        assert!(validate_endpoint_host(&ep("bad_", 443)).is_err());
    }
}
