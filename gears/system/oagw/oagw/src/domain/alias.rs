//! Alias derivation and update enforcement.
//!
//! `cpt-cf-oagw-algo-alias-derive` turns the endpoint pool of an upstream into
//! the tenant-unique `alias`, so an operator never has to invent a name that the
//! router can also derive. `cpt-cf-oagw-algo-alias-update-enforce` keeps that
//! invariant stable across replaces: the alias is immutable, so a replace may
//! only keep it.
//!
//! The derivation is *shape-only*: it reads the endpoints the validator already
//! normalized and never touches the store, so it is a pure domain function.

// @cpt-begin:cpt-cf-oagw-dod-alias-derivation:p1:inst-full
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, ServerConfig};
use crate::domain::validation::is_valid_hostname;

/// Why a set of endpoints does not determine an alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotDerivable {
    /// At least one endpoint is an IP literal, so there is no name to derive.
    IpBased,
    /// The hostnames share no registrable suffix (or only a bare public suffix).
    NoRegistrableSuffix,
}

impl NotDerivable {
    /// The reason rendered for the `400` the mapping layer returns.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::IpBased => "an IP-based endpoint has no derivable name",
            Self::NoRegistrableSuffix => "the endpoints share no registrable domain suffix",
        }
    }
}

/// What the endpoint pool determines about the alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// The pool determines exactly one alias.
    Derived {
        /// The derived alias.
        alias: String,
    },
    /// The pool does not determine an alias; the caller must supply one.
    NotDerivable {
        /// Why the pool does not determine one.
        reason: NotDerivable,
    },
}

// @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-14
/// Derive the alias an endpoint pool determines.
///
/// Pure function of the (already normalized) endpoints, so the create flow can
/// compute the candidate and the replace flow can compare it against the stored
/// alias without consulting the store.
pub fn derive(server: &ServerConfig) -> AliasDerivation {
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-14
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-01
    // Hosts arrive ASCII-lowercased with the trailing dot stripped: the upstream
    // validator normalized them (`inst-uval-02`), so this step is a re-check
    // rather than a mutation.
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-01

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-04
    // The pool rule of the DESIGN: every endpoint of the pool carries the same
    // protocol, scheme and port. A write that mixes them has no single pool and
    // therefore no single alias.
    let Some(first) = server.endpoints.first() else {
        return AliasDerivation::NotDerivable {
            reason: NotDerivable::NoRegistrableSuffix,
        };
    };
    if !server
        .endpoints
        .iter()
        .all(|endpoint| endpoint.scheme == first.scheme && endpoint.port == first.port)
    {
        return AliasDerivation::NotDerivable {
            reason: NotDerivable::NoRegistrableSuffix,
        };
    }
    let suffix_port = port_suffix(first.scheme, first.port);
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-04

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-02
    // An IP literal has no name to derive, so the whole pool is marked IP-based.
    let ip_based = server
        .endpoints
        .iter()
        .any(|endpoint| endpoint.host.parse::<std::net::IpAddr>().is_ok());
    if ip_based {
        return AliasDerivation::NotDerivable {
            reason: NotDerivable::IpBased,
        };
    }
    let names: Vec<&str> = server
        .endpoints
        .iter()
        .map(|endpoint| endpoint.host.as_str())
        .collect();
    if !names.iter().all(|host| is_valid_hostname(host)) {
        return AliasDerivation::NotDerivable {
            reason: NotDerivable::IpBased,
        };
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-02

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-05
    if names.len() == 1 {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-06
        return AliasDerivation::Derived {
            alias: format!("{}{suffix_port}", names[0]),
        };
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-06
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-05

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-07
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-08
    // The longest common suffix of at least two labels, validated against the
    // public suffix list: it must be a registrable domain, not a bare public
    // suffix such as `co.uk`.
    let suffix =
        common_suffix(&names).filter(|candidate| psl::domain_str(candidate) == Some(candidate.as_str()));
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-08
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-09
    match suffix {
        Some(suffix) => AliasDerivation::Derived {
            alias: format!("{suffix}{suffix_port}"),
        },
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-10
        // A bare public suffix or no shared suffix at all is not a name.
        None => AliasDerivation::NotDerivable {
            reason: NotDerivable::NoRegistrableSuffix,
        },
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-10
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-09
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-07
}

/// `:port` when the port is not the standard one for the scheme, else empty.
fn port_suffix(scheme: crate::domain::model::Scheme, port: u16) -> String {
    if port == scheme.default_port() {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// The longest common suffix of at least two labels shared by every host.
///
/// Returns the suffix with its labels joined by dots, or `None` when the hosts
/// share fewer than two labels.
fn common_suffix(hosts: &[&str]) -> Option<String> {
    let label_sets: Vec<Vec<&str>> = hosts
        .iter()
        .map(|host| host.split('.').collect())
        .collect();

    let shortest = label_sets.iter().map(Vec::len).min().unwrap_or(0);
    if shortest < 2 {
        return None;
    }

    // Count the trailing labels every host shares, longest run first.
    let mut shared = 0;
    for offset in 1..=shortest {
        let candidate = label_sets[0][label_sets[0].len() - offset];
        if label_sets
            .iter()
            .all(|labels| labels[labels.len() - offset] == candidate)
        {
            shared = offset;
        } else {
            break;
        }
    }
    if shared < 2 {
        return None;
    }

    let labels = &label_sets[0];
    Some(labels[labels.len() - shared..].join("."))
}

/// Validate the caller-supplied alias against the schema pattern.
///
/// Returns `None` when the value cannot be an alias at all.
#[must_use]
pub fn normalize_alias(supplied: &str) -> Option<String> {
    let lowered = supplied.trim().to_ascii_lowercase();
    let lowered = lowered.strip_suffix('.').unwrap_or(&lowered).to_string();
    matches_alias_pattern(&lowered).then_some(lowered)
}

/// Whether an alias matches `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn matches_alias_pattern(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty()
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
    {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-'))
        && !alias.contains("..")
}

// @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-11
/// Resolve the alias of a new upstream from its pool and the caller's alias.
///
/// This is the create-side half of `cpt-cf-oagw-algo-alias-derive`: a pool that
/// determines no name requires the caller to supply one.
///
/// # Errors
///
/// Returns a [`DomainError`] when the supplied alias differs from the derived
/// one, or when the pool derives nothing and no alias was supplied.
pub fn resolve_alias(server: &ServerConfig, supplied: Option<&str>) -> Result<String, DomainError> {
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-11
    let normalized = supplied.and_then(normalize_alias);
    match derive(server) {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-12
        AliasDerivation::Derived { alias } => match normalized {
            // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-13
            Some(supplied) if supplied == alias => Ok(alias),
            Some(supplied) => Err(DomainError::ValidationError {
                detail: format!(
                    "alias: `{supplied}` differs from the derived alias `{alias}`; omit `alias` \
                     or match it exactly"
                ),
            }),
            // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-13
            None => Ok(alias),
        },
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-ader-12
        AliasDerivation::NotDerivable { reason } => normalized.ok_or_else(|| {
            DomainError::ValidationError {
                detail: format!("alias: required because {}", reason.reason()),
            }
        }),
    }
}

// @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-12
/// Enforce the alias invariant of a replace.
///
/// The alias is immutable, so the replacement always carries the stored one; the
/// only accepted change of `server.endpoints[]` is the one that keeps deriving
/// it.
///
/// # Errors
///
/// Returns a [`DomainError`] describing the rejected transition.
pub fn enforce_alias_update(
    existing: &crate::domain::model::Upstream,
    proposed: &ServerConfig,
    supplied: Option<&str>,
) -> Result<String, DomainError> {
    // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-12
    let normalized = supplied.and_then(normalize_alias);

    // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-01
    let proposed_derivation = derive(proposed);
    // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-01

    // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-02
    if endpoints_equal(&existing.server, proposed) {
        // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-03
        // Endpoints unchanged: the alias stays, and a supplied alias must state
        // exactly the stored one.
        return match normalized {
            Some(supplied) if supplied == existing.alias => Ok(existing.alias.clone()),
            Some(supplied) => Err(rejected_alias(&supplied, &existing.alias)),
            None => Ok(existing.alias.clone()),
        };
        // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-03
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-02

    // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-04
    let existing_derivation = derive(&existing.server);
    match (existing_derivation, proposed_derivation) {
        // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-05
        (AliasDerivation::Derived { .. }, AliasDerivation::Derived { alias }) => {
            if alias == existing.alias {
                Ok(existing.alias.clone())
            } else {
                Err(rejected_alias_recreate(&alias, &existing.alias))
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-05
        // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-06
        (AliasDerivation::Derived { .. }, AliasDerivation::NotDerivable { reason }) => {
            // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-07
            // A derivable upstream cannot become non-derivable: the stored alias
            // would lose its derivation, so even an explicit alias is refused.
            Err(DomainError::ValidationError {
                detail: format!(
                    "alias: `{}` was derived from its endpoints and the proposed endpoints \
                     cannot derive an alias ({}); delete and re-create the upstream",
                    existing.alias,
                    reason.reason()
                ),
            })
            // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-07
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-06
        // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-08
        (AliasDerivation::NotDerivable { .. }, AliasDerivation::NotDerivable { .. }) => {
            // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-09
            // Neither set determines a name: the stored alias stays, and a
            // differing alias is refused.
            match normalized {
                Some(supplied) if supplied == existing.alias => Ok(existing.alias.clone()),
                Some(supplied) => Err(rejected_alias(&supplied, &existing.alias)),
                None => Ok(existing.alias.clone()),
            }
            // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-09
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-08
        // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-10
        (AliasDerivation::NotDerivable { .. }, AliasDerivation::Derived { alias }) => {
            // @cpt-begin:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-11
            // The proposed endpoints derive a name: it must be the stored one,
            // because the alias is immutable even though it is now derivable.
            if alias == existing.alias {
                Ok(existing.alias.clone())
            } else {
                Err(rejected_alias_recreate(&alias, &existing.alias))
            }
            // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-11
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-10
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-update-enforce:p1:inst-aupd-04
}

/// `400` for a supplied alias that differs from the only accepted one.
fn rejected_alias(supplied: &str, expected: &str) -> DomainError {
    DomainError::ValidationError {
        detail: format!(
            "alias: `{supplied}` differs from the required alias `{expected}`; the alias is \
             immutable"
        ),
    }
}

/// `400` for a replace whose endpoints re-derive a different alias.
fn rejected_alias_recreate(derived: &str, existing: &str) -> DomainError {
    DomainError::ValidationError {
        detail: format!(
            "alias: the proposed endpoints derive `{derived}` but the upstream is stored as \
             `{existing}`; the alias is immutable, so delete and re-create the upstream"
        ),
    }
}

/// Whether two endpoint pools are the same set of `scheme://host:port`.
fn endpoints_equal(existing: &ServerConfig, proposed: &ServerConfig) -> bool {
    existing.endpoints.len() == proposed.endpoints.len()
        && existing.endpoints.iter().all(|endpoint| {
            proposed
                .endpoints
                .iter()
                .any(|candidate| same_endpoint(endpoint, candidate))
        })
}

/// Whether two endpoints denote the same dial target.
fn same_endpoint(left: &Endpoint, right: &Endpoint) -> bool {
    left.scheme == right.scheme && left.host == right.host && left.port == right.port
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Scheme, Timestamp};
    use crate::domain::validation::validate_upstream;
    use serde_json::json;
    use uuid::Uuid;

    fn pool(entries: &[(Scheme, &str, u16)]) -> ServerConfig {
        ServerConfig {
            endpoints: entries
                .iter()
                .map(|(scheme, host, port)| Endpoint {
                    scheme: *scheme,
                    host: (*host).to_string(),
                    port: *port,
                })
                .collect(),
        }
    }

    fn derived_alias(entries: &[(Scheme, &str, u16)]) -> Option<String> {
        match derive(&pool(entries)) {
            AliasDerivation::Derived { alias } => Some(alias),
            AliasDerivation::NotDerivable { .. } => None,
        }
    }

    #[test]
    fn a_single_hostname_derives_its_alias_without_the_standard_port() {
        assert_eq!(
            derived_alias(&[(Scheme::Https, "api.vendor.com", 443)]),
            Some("api.vendor.com".to_string()),
            "443 is standard for https"
        );
        assert_eq!(
            derived_alias(&[(Scheme::Http, "api.vendor.com", 80)]),
            Some("api.vendor.com".to_string()),
            "80 is standard for http"
        );
        assert_eq!(
            derived_alias(&[(Scheme::Https, "api.vendor.com", 8443)]),
            Some("api.vendor.com:8443".to_string()),
            "a non-standard port is preserved"
        );
        assert_eq!(
            derived_alias(&[(Scheme::Http, "api.vendor.com", 443)]),
            Some("api.vendor.com:443".to_string()),
            "443 is not standard for http"
        );
    }

    #[test]
    fn the_trailing_dot_is_stripped_and_the_case_lowered() {
        assert_eq!(
            normalize_alias("API.Vendor.COM.").as_deref(),
            Some("api.vendor.com")
        );
        assert_eq!(normalize_alias("-nope-"), None, "the pattern anchors both ends");
        assert_eq!(normalize_alias("ok-1.a:b"), Some("ok-1.a:b".to_string()));
        assert!(matches_alias_pattern("a"));
        assert!(matches_alias_pattern("a-b.c:d"));
        assert!(!matches_alias_pattern("-a"));
        assert!(!matches_alias_pattern("a."));
        assert!(!matches_alias_pattern("a..b"));
        assert!(!matches_alias_pattern(""));
    }

    #[test]
    fn several_hostnames_derive_their_common_registrable_suffix() {
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "us.vendor.com", 443),
                (Scheme::Https, "eu.vendor.com", 443),
            ]),
            Some("vendor.com".to_string()),
            "the DESIGN example"
        );
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "us.vendor.com", 8443),
                (Scheme::Https, "eu.vendor.com", 8443),
            ]),
            Some("vendor.com:8443".to_string()),
            "the non-standard port is preserved on the suffix"
        );
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "a.example.co.uk", 443),
                (Scheme::Https, "b.example.co.uk", 443),
            ]),
            Some("example.co.uk".to_string()),
            "the longest common suffix, not the public suffix"
        );
    }

    #[test]
    fn a_bare_public_suffix_or_no_suffix_is_not_derivable() {
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "a.co.uk", 443),
                (Scheme::Https, "b.co.uk", 443),
            ]),
            None,
            "co.uk is a bare public suffix"
        );
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "one.example.com", 443),
                (Scheme::Https, "other.example.org", 443),
            ]),
            None,
            "no shared suffix"
        );
        assert_eq!(
            derived_alias(&[(Scheme::Https, "api", 443), (Scheme::Https, "metrics", 443)]),
            None,
            "single-label hosts share no two-label suffix"
        );
    }

    #[test]
    fn an_ip_endpoint_is_never_derivable() {
        assert_eq!(
            derived_alias(&[(Scheme::Https, "10.0.0.1", 443)]),
            None,
            "IPv4"
        );
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "10.0.0.1", 443),
                (Scheme::Https, "10.0.0.2", 443),
            ]),
            None,
            "an all-IP pool"
        );
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "api.vendor.com", 443),
                (Scheme::Https, "10.0.0.2", 443),
            ]),
            None,
            "a mixed pool is not derivable either"
        );
    }

    #[test]
    fn endpoints_of_one_pool_must_share_scheme_and_port() {
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "us.vendor.com", 443),
                (Scheme::Wss, "eu.vendor.com", 443),
            ]),
            None,
            "same port, different scheme: no single pool"
        );
        assert_eq!(
            derived_alias(&[
                (Scheme::Https, "us.vendor.com", 443),
                (Scheme::Https, "eu.vendor.com", 8443),
            ]),
            None,
            "same scheme, different port: no single pool"
        );
    }

    #[test]
    fn a_supplied_alias_that_matches_the_derived_one_is_tolerated() {
        let server = pool(&[(Scheme::Https, "api.vendor.com", 443)]);
        assert_eq!(
            resolve_alias(&server, Some("api.vendor.com")).expect("idempotent no-op"),
            "api.vendor.com"
        );
        let error = resolve_alias(&server, Some("payments")).expect_err("differs");
        assert_eq!(error.status(), 400);
        assert!(
            error.detail().contains("api.vendor.com"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn a_non_derivable_pool_requires_an_explicit_alias() {
        let server = pool(&[(Scheme::Https, "10.0.0.1", 443)]);
        assert_eq!(
            resolve_alias(&server, Some("payments-gateway")).expect("explicit alias"),
            "payments-gateway"
        );
        let error = resolve_alias(&server, None).expect_err("no alias");
        assert_eq!(error.status(), 400);
        assert!(error.detail().starts_with("alias:"), "{}", error.detail());
    }

    fn upstream(server: ServerConfig, alias: &str) -> crate::domain::model::Upstream {
        crate::domain::model::Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            enabled: true,
            alias: alias.to_string(),
            tags: Vec::new(),
            server,
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn an_unchanged_endpoint_set_keeps_the_alias() {
        let existing = upstream(pool(&[(Scheme::Https, "api.vendor.com", 443)]), "api.vendor.com");
        let proposed = pool(&[(Scheme::Https, "api.vendor.com", 443)]);
        assert_eq!(
            enforce_alias_update(&existing, &proposed, None).expect("unchanged"),
            "api.vendor.com"
        );
        assert_eq!(
            enforce_alias_update(&existing, &proposed, Some("api.vendor.com"))
                .expect("stating the same alias is accepted"),
            "api.vendor.com"
        );
        let error = enforce_alias_update(&existing, &proposed, Some("other"))
            .expect_err("a differing alias is refused");
        assert!(error.detail().contains("immutable"), "{}", error.detail());
    }

    #[test]
    fn a_derivable_to_derivable_change_must_re_derive_the_same_alias() {
        let existing = upstream(
            pool(&[
                (Scheme::Https, "us.vendor.com", 443),
                (Scheme::Https, "eu.vendor.com", 443),
            ]),
            "vendor.com",
        );
        // A third endpoint of the same registrable domain re-derives the same
        // alias, so the replace is accepted with the alias unchanged.
        let proposed = pool(&[
            (Scheme::Https, "us.vendor.com", 443),
            (Scheme::Https, "eu.vendor.com", 443),
            (Scheme::Https, "ap.vendor.com", 443),
        ]);
        assert_eq!(
            enforce_alias_update(&existing, &proposed, None).expect("same derived alias"),
            "vendor.com"
        );

        // A different registrable domain derives a different alias.
        let conflicting = pool(&[(Scheme::Https, "api.other.com", 443)]);
        let error = enforce_alias_update(&existing, &conflicting, None)
            .expect_err("the alias would change");
        assert!(
            error.detail().contains("delete and re-create"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn a_single_host_upstream_cannot_gain_a_suffix_alias() {
        let existing = upstream(pool(&[(Scheme::Https, "api.vendor.com", 443)]), "api.vendor.com");
        let proposed = pool(&[
            (Scheme::Https, "api.vendor.com", 443),
            (Scheme::Https, "eu.vendor.com", 443),
        ]);
        let error =
            enforce_alias_update(&existing, &proposed, None).expect_err("derives vendor.com");
        assert!(
            error.detail().contains("vendor.com"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn a_derivable_upstream_cannot_become_non_derivable() {
        let existing = upstream(pool(&[(Scheme::Https, "api.vendor.com", 443)]), "api.vendor.com");
        let proposed = pool(&[(Scheme::Https, "10.0.0.1", 443)]);
        for supplied in [None, Some("api.vendor.com"), Some("payments")] {
            let error = enforce_alias_update(&existing, &proposed, supplied)
                .expect_err("not derivable");
            assert!(
                error.detail().contains("cannot derive an alias"),
                "{}",
                error.detail()
            );
        }
    }

    #[test]
    fn two_non_derivable_sets_keep_the_stored_alias() {
        let existing = upstream(pool(&[(Scheme::Https, "10.0.0.1", 443)]), "payments");
        let proposed = pool(&[(Scheme::Https, "10.0.0.2", 443)]);
        assert_eq!(
            enforce_alias_update(&existing, &proposed, None).expect("retained"),
            "payments"
        );
        assert_eq!(
            enforce_alias_update(&existing, &proposed, Some("payments")).expect("same alias"),
            "payments"
        );
        let error = enforce_alias_update(&existing, &proposed, Some("other"))
            .expect_err("differing alias");
        assert!(error.detail().contains("immutable"), "{}", error.detail());
    }

    #[test]
    fn a_non_derivable_set_may_become_derivable_only_with_the_same_alias() {
        let existing = upstream(pool(&[(Scheme::Https, "10.0.0.1", 443)]), "vendor.com");
        let proposed = pool(&[
            (Scheme::Https, "us.vendor.com", 443),
            (Scheme::Https, "eu.vendor.com", 443),
        ]);
        assert_eq!(
            enforce_alias_update(&existing, &proposed, None).expect("same derived alias"),
            "vendor.com"
        );
        let conflicting = pool(&[(Scheme::Https, "api.other.com", 443)]);
        let error = enforce_alias_update(&existing, &conflicting, None)
            .expect_err("a different alias would be derived");
        assert!(
            error.detail().contains("delete and re-create"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn the_validator_and_the_alias_algorithm_agree_on_the_wire_shape() {
        let body = json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "US.Vendor.COM.", "port": 8443 },
                { "scheme": "https", "host": "eu.vendor.com", "port": 8443 }
            ] },
            "protocol": crate::domain::model::PROTOCOL_HTTP
        });
        let spec: crate::domain::validation::UpstreamSpec =
            serde_json::from_value(body).expect("bindable");
        let upstream = validate_upstream(&spec, Uuid::nil(), Timestamp::from_nanos(0))
            .expect("valid");
        assert_eq!(
            resolve_alias(&upstream.server, Some("vendor.com:8443")).expect("matches derivation"),
            "vendor.com:8443"
        );
    }
}

// @cpt-end:cpt-cf-oagw-dod-alias-derivation:p1:inst-full
