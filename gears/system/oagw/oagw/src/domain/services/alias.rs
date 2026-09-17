//! Alias derivation, validation and shadowing (ADR 0003, DESIGN "Alias
//! Resolution").
//!
//! Everything in this module is a **pure function** over endpoints and alias
//! strings so both the control plane (validation on create/replace) and the
//! data plane (resolution at proxy time) can share the exact same rules.
//!
//! # Rules (DESIGN.md "Alias Enforcement Rules")
//!
//! | Endpoint type | Alias rule |
//! |---|---|
//! | Single hostname, standard port | derived: `hostname` |
//! | Single hostname, non-standard port | derived: `hostname:port` |
//! | Multiple hostnames, registrable common suffix | derived: `suffix[:port]` |
//! | Multiple hostnames, common suffix is a bare public suffix | explicit required |
//! | Multiple hostnames, no registrable common suffix | explicit required |
//! | IP addresses (any) | explicit required |
//!
//! Standard ports (omitted from a derived alias): `http` → 80, everything
//! else → 443.

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::parse_gts_instance_id;
use crate::domain::model::upstream::{Endpoint, normalize_host};

/// Maximum length of an RFC 1123 hostname.
pub const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of an RFC 1123 hostname label.
pub const MAX_LABEL_LEN: usize = 63;
/// Maximum length of an alias.
pub const MAX_ALIAS_LEN: usize = 253;
/// Largest port number.
pub const MAX_PORT: u16 = 65_535;

/// Result of deriving an alias from a set of endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedAlias {
    /// An alias could be derived; the value is the alias.
    Derived(String),
    /// No alias could be derived; the operator must supply one.
    NotDerivable(NotDerivable),
}

/// Why derivation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotDerivable {
    /// No endpoints are configured.
    NoEndpoints,
    /// At least one endpoint is a literal IP address.
    IpEndpoint,
    /// A hostname is not RFC 1123 valid.
    InvalidHostname(String),
    /// Hostname pool has no registrable common suffix.
    NoCommonSuffix(Vec<String>),
    /// The only common suffix is a bare public suffix (e.g. `co.uk`).
    BarePublicSuffix(String),
    /// The endpoints disagree on the port, so no single `suffix:port` exists.
    InconsistentPorts(Vec<u16>),
}

impl std::fmt::Display for NotDerivable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEndpoints => f.write_str("the upstream has no endpoints"),
            Self::IpEndpoint => {
                f.write_str("endpoints use literal IP addresses, which cannot yield a hostname")
            }
            Self::InvalidHostname(host) => write!(f, "hostname `{host}` is not a valid DNS name"),
            Self::NoCommonSuffix(hosts) => {
                write!(f, "endpoints {hosts:?} share no common domain suffix")
            }
            Self::BarePublicSuffix(suffix) => write!(
                f,
                "the only common suffix `{suffix}` is a bare public suffix"
            ),
            Self::InconsistentPorts(ports) => write!(
                f,
                "endpoints disagree on the port ({ports:?}), so no single `suffix:port` exists"
            ),
        }
    }
}

impl DerivedAlias {
    /// The derived alias, when derivation succeeded.
    #[must_use]
    pub fn ok(&self) -> Option<&str> {
        match self {
            Self::Derived(alias) => Some(alias),
            Self::NotDerivable(_) => None,
        }
    }

    /// True when an alias was derived.
    #[must_use]
    pub fn is_derived(&self) -> bool {
        matches!(self, Self::Derived(_))
    }
}

/// Normalize an alias: ASCII lower-case, trailing dots stripped, surrounding
/// whitespace removed.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Validate an alias against its wire pattern
/// (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`) and length bound.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ALIAS_LEN {
        return false;
    }
    let is_edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !is_edge(bytes[0]) || !is_edge(bytes[bytes.len() - 1]) {
        return false;
    }
    if bytes.len() == 1 {
        // A single character is a valid alias: there is no middle section.
        return true;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|b| is_edge(*b) || matches!(b, b'.' | b':' | b'-'))
}

/// Validate an RFC 1123 hostname (max 253 chars, labels 1..=63, ASCII
/// alphanumeric plus hyphen, no leading/trailing hyphen, trailing dot
/// tolerated).
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.len() > MAX_HOSTNAME_LEN {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// The registrable-domain suffix shared by every hostname in `hosts`, or
/// `None` when there is none (heterogeneous hosts, or the only common suffix
/// is a bare public suffix).
///
/// `psl` (the Public Suffix List) decides what a registrable domain is, so
/// `foo.co.uk` / `bar.co.uk` yields `None` — `co.uk` is a public suffix, not a
/// registrable domain.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    if hosts.len() < 2 {
        return None;
    }
    let distinct: BTreeSet<&String> = hosts.iter().collect();
    if distinct.len() < 2 {
        return None;
    }
    let mut registrable: Option<String> = None;
    for host in &distinct {
        let r = psl::domain_str(host)?;
        match &registrable {
            None => registrable = Some(r.to_ascii_lowercase()),
            Some(existing) => {
                if !existing.eq_ignore_ascii_case(r) {
                    return None;
                }
            }
        }
    }
    registrable.filter(|r| !hosts.iter().any(|h| h.eq_ignore_ascii_case(r)))
}

/// Compute the alias a set of endpoints implies, without validating anything.
///
/// * no endpoints → [`NotDerivable::NoEndpoints`]
/// * any IP literal → [`NotDerivable::IpEndpoint`]
/// * a single hostname → `hostname` or `hostname:port`
/// * several hostnames → `common_domain_suffix()[:port]` when every port
///   agrees, otherwise [`NotDerivable::InconsistentPorts`]
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> DerivedAlias {
    if endpoints.is_empty() {
        return DerivedAlias::NotDerivable(NotDerivable::NoEndpoints);
    }
    if endpoints.iter().any(Endpoint::is_ip_literal) {
        return DerivedAlias::NotDerivable(NotDerivable::IpEndpoint);
    }

    let mut hosts = Vec::with_capacity(endpoints.len());
    let mut ports = BTreeSet::new();
    for endpoint in endpoints {
        let host = normalize_host(&endpoint.host);
        if !is_valid_hostname(&host) {
            return DerivedAlias::NotDerivable(NotDerivable::InvalidHostname(
                endpoint.host.clone(),
            ));
        }
        ports.insert(endpoint.effective_port());
        hosts.push(host);
    }
    if ports.len() > 1 {
        return DerivedAlias::NotDerivable(NotDerivable::InconsistentPorts(
            ports.into_iter().collect(),
        ));
    }
    let port = ports.iter().copied().next().unwrap_or_default();
    let standard = endpoints
        .first()
        .is_some_and(|e| port == e.scheme.default_port());

    if hosts.len() == 1 {
        let host = &hosts[0];
        return if standard {
            DerivedAlias::Derived(host.clone())
        } else {
            DerivedAlias::Derived(format!("{host}:{port}"))
        };
    }

    match common_domain_suffix(&hosts) {
        Some(suffix) if !suffix.contains(':') && !suffix.is_empty() => {
            if standard {
                DerivedAlias::Derived(suffix)
            } else {
                DerivedAlias::Derived(format!("{suffix}:{port}"))
            }
        }
        Some(_) => DerivedAlias::NotDerivable(NotDerivable::BarePublicSuffix(hosts.join(","))),
        None => DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix(hosts)),
    }
}

/// Resolve the alias of an upstream: derivation when possible, otherwise the
/// explicit alias.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the endpoints are non-derivable
/// and no alias was supplied, when the supplied alias is malformed, or when a
/// hostname-derived upstream was given a different alias.
pub fn enforce_alias_on_create(
    endpoints: &[Endpoint],
    supplied_alias: Option<&str>,
) -> Result<String, DomainError> {
    match compute_derived_alias(endpoints) {
        DerivedAlias::Derived(derived) => match supplied_alias.map(normalize_alias) {
            None => Ok(derived),
            Some(given) if given == derived => Ok(derived),
            Some(given) => Err(DomainError::validation(
                "alias",
                format!(
                    "alias `{given}` does not match the derived alias `{derived}` for these \
                     hostname endpoints; hostname-based upstreams derive their alias"
                ),
            )),
        },
        DerivedAlias::NotDerivable(reason) => {
            let Some(alias) = supplied_alias
                .map(normalize_alias)
                .filter(|a| !a.is_empty())
            else {
                return Err(DomainError::validation(
                    "alias",
                    format!(
                        "explicit alias is required: {reason} (see ADR 0003 alias enforcement rules)"
                    ),
                ));
            };
            if !is_valid_alias(&alias) {
                return Err(DomainError::validation(
                    "alias",
                    format!("alias `{alias}` must match `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`"),
                ));
            }
            Ok(alias)
        }
    }
}

/// Enforce alias immutability on replace (ADR 0003 alias update matrix).
///
/// `existing_alias` is the alias currently stored, `endpoints` the *new*
/// endpoints and `supplied_alias` the (optional) alias from the request body.
///
/// * endpoints unchanged → the existing alias is retained; a differing alias
///   is rejected as an override attempt.
/// * endpoints changed and still derivable → the recomputed alias must equal
///   the existing one, otherwise the update is rejected.
/// * endpoints changed and no longer derivable → rejected, always (even when
///   an explicit alias is provided).
///
/// # Errors
/// Returns [`DomainError::AliasImmutable`] when the transition is not
/// permitted and [`DomainError::Validation`] when the supplied alias is
/// malformed.
pub fn enforce_alias_update(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    endpoints: &[Endpoint],
    supplied_alias: Option<&str>,
) -> Result<String, DomainError> {
    let endpoints_unchanged = same_endpoints(existing_endpoints, endpoints);
    let given = supplied_alias
        .map(normalize_alias)
        .filter(|a| !a.is_empty());

    if let Some(given) = &given
        && !is_valid_alias(given)
    {
        return Err(DomainError::validation(
            "alias",
            format!("alias `{given}` must match `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`"),
        ));
    }

    if endpoints_unchanged {
        return match given {
            None => Ok(existing_alias.to_owned()),
            Some(given) if given == existing_alias => Ok(existing_alias.to_owned()),
            Some(given) => Err(DomainError::AliasImmutable {
                detail: format!(
                    "alias is immutable and `{given}` would replace `{existing_alias}`; \
                     delete and re-create the upstream instead"
                ),
            }),
        };
    }

    match compute_derived_alias(endpoints) {
        DerivedAlias::Derived(derived) => {
            if derived == existing_alias {
                Ok(existing_alias.to_owned())
            } else {
                Err(DomainError::AliasImmutable {
                    detail: format!(
                        "changing the endpoints would change the alias from `{existing_alias}` \
                         to `{derived}`; the alias is the routing key and is immutable — delete \
                         and re-create the upstream instead"
                    ),
                })
            }
        }
        DerivedAlias::NotDerivable(_) => {
            // DESIGN "Alias Update Behavior": `Non-derivable → Non-derivable`
            // (an IP pool re-pointed at another IP pool) retains the existing
            // alias; only a *differing* explicit alias is refused. A
            // `Derivable → Non-derivable` transition is rejected in every case,
            // because it would strip the derived routing key.
            let was_derivable = !matches!(
                compute_derived_alias(existing_endpoints),
                DerivedAlias::NotDerivable(_)
            );
            let retained =
                !was_derivable && given.as_deref().is_none_or(|given| given == existing_alias);
            if retained {
                return Ok(existing_alias.to_owned());
            }
            let detail = if was_derivable {
                format!(
                    "the endpoints no longer derive `{existing_alias}` and the alias is immutable; \
                     delete and re-create the upstream instead"
                )
            } else {
                format!(
                    "alias is immutable and `{}` would replace `{existing_alias}`; \
                     delete and re-create the upstream instead",
                    given.as_deref().unwrap_or(existing_alias)
                )
            };
            Err(DomainError::AliasImmutable { detail })
        }
    }
}

/// True when two endpoint sets are equivalent for alias purposes (same
/// normalised `host:port` set, ignoring order and duplicates).
#[must_use]
pub fn same_endpoints(a: &[Endpoint], b: &[Endpoint]) -> bool {
    let key = |eps: &[Endpoint]| -> BTreeSet<String> {
        eps.iter()
            .map(|e| {
                let port = e.effective_port();
                format!("{}:{}", normalize_host(&e.host), port)
            })
            .collect()
    };
    key(a) == key(b)
}

/// One entry of the tenant chain considered during alias resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasCandidate {
    /// Tenant that owns the upstream.
    pub tenant_id: Uuid,
    /// Upstream alias (already normalised).
    pub alias: String,
    /// Upstream id.
    pub upstream_id: Uuid,
    /// Whether the upstream is enabled.
    pub enabled: bool,
}

/// Result of alias shadowing across a tenant chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasResolution {
    /// The closest enabled upstream in the chain.
    Resolved(AliasCandidate),
    /// The alias exists in the chain but every candidate is disabled.
    Disabled(Vec<AliasCandidate>),
    /// The alias does not exist anywhere in the chain.
    Unresolved,
}

impl AliasResolution {
    /// The resolved upstream, when any.
    #[must_use]
    pub fn upstream_id(&self) -> Option<Uuid> {
        match self {
            Self::Resolved(c) => Some(c.upstream_id),
            _ => None,
        }
    }
}

/// Resolve an alias across a tenant chain (descendant first).
///
/// `candidates` must already be ordered descendant → root. The **closest
/// enabled** match wins (DESIGN "Shadowing Behavior"); ancestors are only
/// consulted when no enabled descendant owns the alias. A disabled candidate is
/// skipped, and when *no* enabled candidate exists the disabled ones are
/// reported as [`AliasResolution::Disabled`] so the caller can answer 503
/// rather than 404.
#[must_use]
pub fn resolve_alias<'a, I>(alias: &str, candidates: I) -> AliasResolution
where
    I: IntoIterator<Item = &'a AliasCandidate>,
{
    let alias = normalize_alias(alias);
    let mut disabled = Vec::new();
    for candidate in candidates {
        if !candidate.alias.eq_ignore_ascii_case(&alias) {
            continue;
        }
        if candidate.enabled {
            return AliasResolution::Resolved(candidate.clone());
        }
        disabled.push(candidate.clone());
    }
    if disabled.is_empty() {
        AliasResolution::Unresolved
    } else {
        AliasResolution::Disabled(disabled)
    }
}

/// Parse a wire alias segment from a proxy path into its canonical form.
///
/// Rejects empty segments and aliases that are not valid after normalisation.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the alias is empty or malformed.
pub fn parse_proxy_alias(raw: &str) -> Result<String, DomainError> {
    let alias = normalize_alias(raw);
    if alias.is_empty() {
        return Err(DomainError::validation("alias", "alias must not be empty"));
    }
    if !is_valid_alias(&alias) {
        return Err(DomainError::validation(
            "alias",
            format!("alias `{alias}` must match `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`"),
        ));
    }
    Ok(alias)
}

/// Extract the upstream id a `PluginRef` points at, when it is UUID-backed.
#[must_use]
pub fn plugin_uuid_from_ref(plugin_ref: &str) -> Option<Uuid> {
    parse_gts_instance_id(plugin_ref).filter(|_| !plugin_ref.contains('~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(host: &str, scheme: crate::domain::model::Scheme, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port() {
        let eps = [ep(
            "api.openai.com",
            crate::domain::model::Scheme::Https,
            None,
        )];
        assert_eq!(
            compute_derived_alias(&eps),
            DerivedAlias::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn single_hostname_non_standard_port() {
        let eps = [ep(
            "api.openai.com",
            crate::domain::model::Scheme::Https,
            Some(8443),
        )];
        assert_eq!(
            compute_derived_alias(&eps),
            DerivedAlias::Derived("api.openai.com:8443".to_owned())
        );
    }

    #[test]
    fn http_defaults_to_port_80() {
        let eps = [ep(
            "api.openai.com",
            crate::domain::model::Scheme::Http,
            None,
        )];
        assert_eq!(
            compute_derived_alias(&eps),
            DerivedAlias::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn common_registrable_suffix() {
        let eps = [
            ep("us.vendor.com", crate::domain::model::Scheme::Https, None),
            ep("eu.vendor.com", crate::domain::model::Scheme::Https, None),
        ];
        assert_eq!(
            compute_derived_alias(&eps),
            DerivedAlias::Derived("vendor.com".to_owned())
        );
    }

    #[test]
    fn common_suffix_with_port() {
        let eps = [
            ep(
                "us.vendor.com",
                crate::domain::model::Scheme::Https,
                Some(8443),
            ),
            ep(
                "eu.vendor.com",
                crate::domain::model::Scheme::Https,
                Some(8443),
            ),
        ];
        assert_eq!(
            compute_derived_alias(&eps),
            DerivedAlias::Derived("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let eps = [
            ep("foo.co.uk", crate::domain::model::Scheme::Https, None),
            ep("bar.co.uk", crate::domain::model::Scheme::Https, None),
        ];
        assert!(matches!(
            compute_derived_alias(&eps),
            DerivedAlias::NotDerivable(_)
        ));
    }

    #[test]
    fn unrelated_hostnames_are_not_derivable() {
        let eps = [
            ep("us.foo.com", crate::domain::model::Scheme::Https, None),
            ep("eu.bar.com", crate::domain::model::Scheme::Https, None),
        ];
        assert!(matches!(
            compute_derived_alias(&eps),
            DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix(_))
        ));
    }

    #[test]
    fn ip_addresses_require_an_explicit_alias() {
        let eps = [
            ep("10.0.1.1", crate::domain::model::Scheme::Https, None),
            ep("10.0.1.2", crate::domain::model::Scheme::Https, None),
        ];
        assert!(matches!(
            compute_derived_alias(&eps),
            DerivedAlias::NotDerivable(NotDerivable::IpEndpoint)
        ));
        let err = enforce_alias_on_create(&eps, None).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn differing_explicit_alias_on_hostname_is_rejected() {
        let eps = [ep(
            "api.openai.com",
            crate::domain::model::Scheme::Https,
            None,
        )];
        assert!(enforce_alias_on_create(&eps, Some("my-service")).is_err());
        // Idempotent no-op: supplying the derived value is tolerated.
        assert_eq!(
            enforce_alias_on_create(&eps, Some("api.openai.com")).unwrap(),
            "api.openai.com"
        );
        assert_eq!(
            enforce_alias_on_create(&eps, None).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn alias_update_matrix() {
        let host = [ep(
            "api.openai.com",
            crate::domain::model::Scheme::Https,
            None,
        )];
        let ip = [ep("10.0.1.1", crate::domain::model::Scheme::Https, None)];

        // Derivable → Derivable, alias would change: rejected.
        let other = [ep(
            "api.other.com",
            crate::domain::model::Scheme::Https,
            None,
        )];
        let err = enforce_alias_update("api.openai.com", &host, &other, None).unwrap_err();
        assert_eq!(err.status(), 400);

        // Derivable → Derivable, same alias: allowed.
        assert_eq!(
            enforce_alias_update("api.openai.com", &host, &host, None).unwrap(),
            "api.openai.com"
        );

        // Derivable → Non-derivable: rejected.
        assert!(enforce_alias_update("api.openai.com", &host, &ip, Some("my-service")).is_err());

        // Non-derivable → Non-derivable, same alias: retained.
        let ip2 = [ep("10.0.1.2", crate::domain::model::Scheme::Https, None)];
        assert_eq!(
            enforce_alias_update("my-service", &ip, &ip2, None).unwrap(),
            "my-service"
        );
        // Non-derivable → Non-derivable, differing alias: rejected.
        assert!(enforce_alias_update("my-service", &ip, &ip2, Some("other")).is_err());

        // No endpoint change, exact-match alias: tolerated.
        assert_eq!(
            enforce_alias_update("my-service", &ip, &ip, Some("my-service")).unwrap(),
            "my-service"
        );
    }

    #[test]
    fn shadowing_picks_the_closest_enabled_upstream() {
        let root = AliasCandidate {
            tenant_id: Uuid::nil(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let child = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let chain = [child.clone(), root.clone()];
        assert_eq!(
            resolve_alias("api.vendor.com", &chain),
            AliasResolution::Resolved(child.clone())
        );
        // Case-insensitive resolution.
        assert_eq!(
            resolve_alias("API.Vendor.COM", &chain),
            AliasResolution::Resolved(child)
        );
    }

    #[test]
    fn a_disabled_closest_match_does_not_fall_through() {
        let root = AliasCandidate {
            tenant_id: Uuid::nil(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let off = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: false,
        };
        // A disabled candidate does not shadow: the closest *enabled* upstream
        // in the chain wins (DESIGN "Shadowing Behavior").
        assert_eq!(
            resolve_alias("api.vendor.com", [&off, &root]),
            AliasResolution::Resolved(root.clone())
        );
        // Only when no enabled candidate exists is the alias reported as
        // disabled, so the caller can answer 503 instead of 404.
        let only = [&off];
        assert_eq!(
            resolve_alias("api.vendor.com", only),
            AliasResolution::Disabled(vec![off])
        );
        assert_eq!(
            resolve_alias("missing.alias", Vec::<&AliasCandidate>::new()),
            AliasResolution::Unresolved
        );
    }

    #[test]
    fn an_ancestor_is_consulted_when_the_child_owns_a_different_alias() {
        let root = AliasCandidate {
            tenant_id: Uuid::nil(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let parent = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let child = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "child.internal".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        // The child owns a *different* alias, so it does not shadow the
        // inherited one: the nearest ancestor that owns the alias wins.
        let chain = [child.clone(), parent.clone(), root.clone()];
        assert_eq!(
            resolve_alias("api.vendor.com", &chain),
            AliasResolution::Resolved(parent)
        );
        // …and the child's own alias still resolves to the child.
        assert_eq!(
            resolve_alias("child.internal", &chain),
            AliasResolution::Resolved(child)
        );
    }

    #[test]
    fn the_nearest_ancestor_wins_when_several_own_the_alias() {
        let root = AliasCandidate {
            tenant_id: Uuid::nil(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let grandparent = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let parent = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        let nearest_tenant = parent.tenant_id;
        let chain = [parent, grandparent, root];
        assert!(matches!(
            resolve_alias("api.vendor.com", &chain),
            AliasResolution::Resolved(ref nearest) if nearest.tenant_id == nearest_tenant
        ));
    }

    #[test]
    fn a_disabled_ancestor_does_not_shadow_an_enabled_descendant_of_the_same_alias() {
        // Reversed order: an enabled *descendant* is reachable even though an
        // ancestor closer to the root disabled its own definition.
        let ancestor_off = AliasCandidate {
            tenant_id: Uuid::nil(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: false,
        };
        let child = AliasCandidate {
            tenant_id: Uuid::new_v4(),
            alias: "api.vendor.com".to_owned(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
        };
        assert_eq!(
            resolve_alias("api.vendor.com", [&ancestor_off, &child]),
            AliasResolution::Resolved(child)
        );
    }

    #[test]
    fn hostname_validation() {
        assert!(is_valid_hostname("api.openai.com"));
        assert!(is_valid_hostname("api.openai.com."));
        assert!(is_valid_hostname("10.0.1.1"));
        assert!(!is_valid_hostname("-api.openai.com"));
        assert!(is_valid_hostname("a-b.c-d.com"));
        assert!(!is_valid_hostname("a_b.com"));
        assert!(!is_valid_hostname(""));
    }

    #[test]
    fn alias_validation() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("a"));
        assert!(is_valid_alias("vendor.com:8443"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("-api"));
        assert!(!is_valid_alias("Api.openai.com"));
    }

    #[test]
    fn proxy_alias_is_normalized() {
        assert_eq!(
            parse_proxy_alias("Api.OpenAI.COM").unwrap(),
            "api.openai.com"
        );
        assert!(parse_proxy_alias("").is_err());
        assert!(parse_proxy_alias("/bad").is_err());
    }
}
