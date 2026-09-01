// Created: 2026-08-31 by Constructor Tech
//! Alias derivation and the alias-immutability rules (DESIGN §3.2
//! "Alias Resolution").
//!
//! The module is deliberately free of HTTP types: it maps an endpoint pool to
//! a routing key, and decides whether a caller-supplied alias may coexist with
//! that key. [`crate::domain::service`] turns the rejections into 400
//! validation problems.

use std::net::IpAddr;

use crate::domain::model::{Endpoint, Scheme};

/// Standard ports omitted from a derived alias (DESIGN §3.2).
const fn is_standard_port(scheme: Scheme, port: u16) -> bool {
    scheme.default_port() == port
}

/// Classification of an endpoint host (DESIGN §3.2 "Hostname Validation").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// IPv4 or IPv6 literal — never contributes to a derived alias.
    IpAddress,
    /// RFC 1123 hostname.
    Hostname,
    /// Neither: rejected before the record is stored.
    Invalid,
}

/// Strip the FQDN trailing dot and ASCII-lowercase a host or alias.
#[must_use]
pub fn normalize(raw: &str) -> String {
    raw.trim().trim_end_matches('.').trim().to_ascii_lowercase()
}

/// Normalise a caller-supplied alias: lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(raw: &str) -> String {
    normalize(raw)
}

/// Validate a derived or explicit alias (DESIGN §3.2 + upstream schema).
///
/// The alias is the routing key of `/v1/proxy/{alias}/...` and becomes
/// `Host` material for the upstream request, so it must be a strict LDH name:
/// lowercase ASCII alphanumeric labels joined by dots, each label not starting
/// or ending with a hyphen, optionally carrying `:port`. Everything else —
/// path separators, whitespace, `%` escapes, control characters, empty labels,
/// consecutive dots — is rejected before a record is stored.
///
/// # Errors
/// The rejection reason as a human-readable message; the caller (write path)
/// turns it into a 400.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("alias must not be empty".to_owned());
    }
    if alias.len() > 253 {
        return Err("alias exceeds 253 characters".to_owned());
    }
    if !alias.is_ascii() {
        return Err("alias must be ASCII".to_owned());
    }
    if alias.contains(' ') || alias.chars().any(char::is_control) {
        return Err("alias must not contain whitespace or control characters".to_owned());
    }
    let (host, port) = split_alias_port(alias)?;
    if port.is_some_and(|port| port == 0) {
        return Err("alias port must be between 1 and 65535".to_owned());
    }
    if !is_ldh_host(host) {
        return Err(format!(
            "alias '{alias}' must be a lowercase LDH hostname (optionally ':port')"
        ));
    }
    Ok(())
}

/// Split an `host:port` alias; the port part is optional.
///
/// Only the *last* colon splits, so a bare IPv6 literal without brackets is
/// still rejected by the LDH check (bracketed IPv6 aliases are not supported
/// in v1 because the proxy path segment would need escaping).
fn split_alias_port(alias: &str) -> Result<(&str, Option<u16>), String> {
    match alias.rsplit_once(':') {
        Some((host, port)) => {
            let parsed = port
                .parse::<u16>()
                .map_err(|_| format!("alias port '{port}' is not a number between 1 and 65535"))?;
            Ok((host, Some(parsed)))
        }
        None => Ok((alias, None)),
    }
}

/// Whether `host` is a strict LDH name: lowercase alphanumeric labels joined
/// by single dots, no empty label, no leading/trailing hyphen or dot.
fn is_ldh_host(host: &str) -> bool {
    !host.is_empty()
        && !host.contains("..")
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
                })
        })
}

/// Validate a hostname per RFC 1123 (labels 1-63, alphanumeric plus hyphen,
/// no leading/trailing hyphen, at most 253 characters, trailing dot allowed).
///
/// # Errors
/// The violated rule as a human-readable message.
pub fn validate_hostname(hostname: &str) -> Result<(), String> {
    if hostname.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    if hostname.len() > 253 {
        return Err("host exceeds 253 characters".to_owned());
    }
    if !hostname.is_ascii() {
        return Err("host must be ASCII".to_owned());
    }
    let labels: Vec<&str> = hostname.split('.').collect();
    let labels: Vec<&str> = if labels.last().is_some_and(|last| last.is_empty()) {
        labels[..labels.len() - 1].to_vec()
    } else {
        labels
    };
    if labels.is_empty() {
        return Err("host must contain at least one label".to_owned());
    }
    for label in labels {
        if label.is_empty() {
            return Err("host contains an empty label".to_owned());
        }
        if label.len() > 63 {
            return Err("host label exceeds 63 characters".to_owned());
        }
        let valid = label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
        if !valid {
            return Err(format!("host label '{label}' contains invalid characters"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "host label '{label}' has a leading or trailing hyphen"
            ));
        }
    }
    Ok(())
}

/// Classify an endpoint host.
#[must_use]
pub fn classify_host(host: &str) -> HostKind {
    let candidate = normalize(host);
    if candidate.parse::<IpAddr>().is_ok() {
        return HostKind::IpAddress;
    }
    if validate_hostname(&candidate).is_ok() {
        return HostKind::Hostname;
    }
    HostKind::Invalid
}

/// Why an endpoint pool has no derived alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotDerivableReason {
    /// Every endpoint is an IP literal.
    IpEndpoints,
    /// Heterogeneous hostnames with no shared registrable suffix.
    NoCommonSuffix,
    /// The shared suffix is a bare public suffix (`co.uk`), so no registrable
    /// suffix exists to route on.
    BarePublicSuffix,
}

impl NotDerivableReason {
    /// Operator-facing explanation.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            NotDerivableReason::IpEndpoints => "IP-based endpoints require an explicit alias",
            NotDerivableReason::NoCommonSuffix => {
                "endpoints share no registrable common suffix; an explicit alias is required"
            }
            NotDerivableReason::BarePublicSuffix => {
                "alias host has no registrable suffix; an explicit alias is required"
            }
        }
    }
}

/// Rejection reasons produced while settling an alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasRejection {
    /// Endpoints cannot produce an alias; the caller must supply one.
    ExplicitRequired(NotDerivableReason),
    /// Caller-supplied alias differs from the derived one.
    DerivedMismatch {
        /// Alias the endpoint pool implies.
        derived: String,
    },
    /// An update would move the routing key.
    ChangeRejected {
        /// Current routing key.
        existing: String,
        /// Routing key the new endpoints imply, when derivable.
        derived: Option<String>,
    },
    /// Explicit alias fails syntax validation.
    InvalidAlias(String),
}

impl AliasRejection {
    /// Operator-facing explanation.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            AliasRejection::ExplicitRequired(reason) => reason.detail().to_owned(),
            AliasRejection::DerivedMismatch { derived } => {
                format!("alias must match the value derived from the endpoints ('{derived}')")
            }
            AliasRejection::ChangeRejected { existing, derived } => match derived {
                Some(derived) => format!(
                    "endpoint change would alter the derived alias from '{existing}' to \
                     '{derived}'; delete and re-create the upstream instead"
                ),
                None => format!(
                    "endpoint change would invalidate the derived alias '{existing}'; \
                     delete and re-create the upstream instead"
                ),
            },
            AliasRejection::InvalidAlias(detail) => detail.clone(),
        }
    }
}

/// Routing key of a single endpoint: `host`, or `host:port` on a
/// non-standard port.
#[must_use]
pub fn endpoint_alias_key(endpoint: &Endpoint) -> String {
    let host = normalize(&endpoint.host);
    if is_standard_port(endpoint.scheme, endpoint.port) {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    }
}

/// Longest common suffix of a label list, in labels.
fn common_label_suffix(hosts: &[&str]) -> Option<String> {
    let mut lists = hosts
        .iter()
        .map(|host| host.split('.').collect::<Vec<&str>>());
    let first = lists.next()?;
    let mut shared = first.len();
    for labels in lists {
        let mut matched = 0;
        while matched < shared
            && matched < labels.len()
            && labels[labels.len() - 1 - matched] == first[first.len() - 1 - matched]
        {
            matched += 1;
        }
        shared = matched;
    }
    if shared < 2 {
        return None;
    }
    Some(first[first.len() - shared..].join("."))
}

/// Whether `candidate` carries a registrable suffix (PSL-validated).
///
/// A derived suffix may sit *below* the registrable domain: `us.vendor.com`
/// is a perfectly usable routing key even though `vendor.com` is the
/// registrable one. Only names with no registrable suffix at all — bare
/// public suffixes such as `co.uk`, unknown TLDs — are rejected.
fn is_registrable_suffix(candidate: &str) -> bool {
    psl::domain_str(candidate).is_some()
}

/// Derive the alias of an endpoint pool (DESIGN §3.2 table).
///
/// * one distinct host → `host` (or `host:port`), hostname or IP literal alike
/// * several hostnames with a registrable common suffix → that suffix
///   (carrying `:port` when the pool runs on a non-standard port)
/// * IP *pools*, bare public suffixes and unrelated hostnames →
///   [`AliasRejection`] (an explicit alias is required)
///
/// # Errors
/// [`NotDerivableReason`] when no alias can be derived from the pool alone.
pub fn derive_alias(endpoints: &[Endpoint]) -> Result<String, NotDerivableReason> {
    if endpoints.is_empty() {
        return Err(NotDerivableReason::NoCommonSuffix);
    }
    let normalized: Vec<String> = endpoints.iter().map(|e| normalize(&e.host)).collect();
    let ip_count = normalized
        .iter()
        .filter(|host| host.parse::<IpAddr>().is_ok())
        .count();
    // A single distinct host is always derivable, IP literal included: the
    // management API accepts `{"scheme":"http","host":"127.0.0.1","port":80}`
    // without an explicit alias.
    let mut distinct = normalized.clone();
    distinct.sort();
    distinct.dedup();
    if distinct.len() == 1 {
        return Ok(endpoint_alias_key(&endpoints[0]));
    }
    if ip_count == normalized.len() {
        return Err(NotDerivableReason::IpEndpoints);
    }
    if ip_count > 0 {
        return Err(NotDerivableReason::NoCommonSuffix);
    }
    let Some(suffix) =
        common_label_suffix(&normalized.iter().map(String::as_str).collect::<Vec<_>>())
    else {
        return Err(NotDerivableReason::NoCommonSuffix);
    };
    if !is_registrable_suffix(&suffix) {
        return Err(NotDerivableReason::BarePublicSuffix);
    }
    let first = &endpoints[0];
    if endpoints
        .iter()
        .all(|endpoint| endpoint.port == first.port && endpoint.scheme == first.scheme)
        && !is_standard_port(first.scheme, first.port)
    {
        return Ok(format!("{suffix}:{}", first.port));
    }
    Ok(suffix)
}

/// Settle the alias of a **new** upstream (DESIGN §3.2).
///
/// Hostname pools always win over a caller-supplied value; the exact derived
/// value is tolerated for idempotency. IP-based and otherwise non-derivable
/// pools require an explicit alias.
///
/// # Errors
/// [`AliasRejection::DerivedMismatch`] when the caller-supplied alias differs
/// from the derived one, [`AliasRejection::ExplicitRequired`] when the pool
/// derives nothing and no alias was supplied, [`AliasRejection::InvalidAlias`]
/// when the supplied alias breaks the LDH rules.
pub fn resolve_creation_alias(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, AliasRejection> {
    let provided = provided
        .map(normalize_alias)
        .filter(|alias| !alias.is_empty());
    match derive_alias(endpoints) {
        Ok(derived) => match provided {
            Some(candidate) if candidate == derived => Ok(derived),
            Some(_) => Err(AliasRejection::DerivedMismatch { derived }),
            None => Ok(derived),
        },
        Err(reason) => match provided {
            Some(candidate) => {
                validate_alias(&candidate).map_err(AliasRejection::InvalidAlias)?;
                Ok(candidate)
            }
            None => Err(AliasRejection::ExplicitRequired(reason)),
        },
    }
}

/// Enforce alias immutability on an update (DESIGN §3.2 table).
///
/// `current_endpoints` is the stored pool, `next_endpoints` the replacement,
/// `provided` an explicitly supplied alias (usually `None`, because the alias
/// is not part of the update DTO).
///
/// # Errors
/// [`AliasRejection`] when the alias would change (a pool move that renames it,
/// a differing explicit value) or the supplied value is not a valid alias.
pub fn enforce_update_alias(
    existing_alias: &str,
    current_endpoints: &[Endpoint],
    next_endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<(), AliasRejection> {
    let provided = provided
        .map(normalize_alias)
        .filter(|alias| !alias.is_empty());
    let pool_unchanged = endpoint_keys(current_endpoints) == endpoint_keys(next_endpoints);

    if pool_unchanged {
        return match provided {
            Some(candidate) if candidate == existing_alias => Ok(()),
            Some(_) => Err(AliasRejection::ChangeRejected {
                existing: existing_alias.to_owned(),
                derived: None,
            }),
            None => Ok(()),
        };
    }

    let next_derived = derive_alias(next_endpoints);

    // Derivable -> non-derivable is rejected unconditionally (DESIGN §3.2
    // "Alias Update Behavior"): the routing key cannot be re-derived and an
    // operator-supplied alias does not rescue the transition.
    if let Err(reason) = &next_derived
        && derive_alias(current_endpoints).is_ok()
    {
        return Err(AliasRejection::ExplicitRequired(*reason));
    }

    match provided {
        Some(candidate) if candidate != existing_alias => {
            return Err(AliasRejection::ChangeRejected {
                existing: existing_alias.to_owned(),
                derived: next_derived.ok(),
            });
        }
        _ => {}
    }

    match next_derived {
        Ok(derived) if derived == existing_alias => Ok(()),
        Ok(derived) => Err(AliasRejection::ChangeRejected {
            existing: existing_alias.to_owned(),
            derived: Some(derived),
        }),
        // Non-derivable -> non-derivable keeps the existing routing key.
        Err(_) => Ok(()),
    }
}

/// Sorted alias keys of a pool, so endpoint order does not affect the
/// "no endpoint change" branch.
fn endpoint_keys(endpoints: &[Endpoint]) -> Vec<String> {
    let mut keys: Vec<String> = endpoints.iter().map(endpoint_alias_key).collect();
    keys.sort();
    keys
}

#[cfg(test)]
mod tests {
    use crate::domain::alias::{
        AliasRejection, NotDerivableReason, classify_host, derive_alias, endpoint_alias_key,
        enforce_update_alias, normalize, resolve_creation_alias, validate_alias, validate_hostname,
    };
    use crate::domain::model::{Endpoint, Scheme};

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn row_one_hostname_with_standard_port_derives_the_hostname() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(derive_alias(&endpoints), Ok("api.openai.com".to_owned()));
    }

    #[test]
    fn row_two_hostname_with_non_standard_port_derives_host_and_port() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            derive_alias(&endpoints),
            Ok("api.openai.com:8443".to_owned())
        );
    }

    #[test]
    fn row_three_multiple_hostnames_derive_the_registrable_suffix() {
        let endpoints = [
            endpoint(Scheme::Https, "us.vendor.com", 443),
            endpoint(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(derive_alias(&endpoints), Ok("vendor.com".to_owned()));
    }

    #[test]
    fn row_four_bare_public_suffix_is_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "foo.co.uk", 443),
            endpoint(Scheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(
            derive_alias(&endpoints),
            Err(NotDerivableReason::BarePublicSuffix)
        );
    }

    #[test]
    fn row_five_unrelated_hostnames_are_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "us.foo.com", 443),
            endpoint(Scheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(
            derive_alias(&endpoints),
            Err(NotDerivableReason::NoCommonSuffix)
        );
    }

    #[test]
    fn row_six_ip_addresses_are_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(
            derive_alias(&endpoints),
            Err(NotDerivableReason::IpEndpoints)
        );
    }

    #[test]
    fn suffix_deeper_than_the_registrable_domain_is_derivable() {
        // `us.vendor.com` is not itself the registrable domain (`vendor.com`
        // is), but it carries a registrable suffix and is therefore a usable
        // routing key.
        let endpoints = [
            endpoint(Scheme::Https, "x.us.vendor.com", 443),
            endpoint(Scheme::Https, "y.us.vendor.com", 443),
        ];
        assert_eq!(derive_alias(&endpoints), Ok("us.vendor.com".to_owned()));
    }

    #[test]
    fn a_single_private_suffix_host_is_derivable() {
        let endpoints = [endpoint(Scheme::Https, "svc.eu.platform.sh", 443)];
        assert_eq!(
            derive_alias(&endpoints),
            Ok("svc.eu.platform.sh".to_owned())
        );
    }

    #[test]
    fn suffix_derivation_keeps_a_shared_non_standard_port() {
        let endpoints = [
            endpoint(Scheme::Https, "us.vendor.com", 8443),
            endpoint(Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(derive_alias(&endpoints), Ok("vendor.com:8443".to_owned()));
    }

    #[test]
    fn mixed_hostname_and_ip_pool_is_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "us.vendor.com", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(
            derive_alias(&endpoints),
            Err(NotDerivableReason::NoCommonSuffix)
        );
    }

    #[test]
    fn endpoint_keys_use_the_scheme_default_port() {
        assert_eq!(
            endpoint_alias_key(&endpoint(Scheme::Http, "EXAMPLE.com.", 80)),
            "example.com"
        );
        assert_eq!(
            endpoint_alias_key(&endpoint(Scheme::Ws, "example.com", 8080)),
            "example.com:8080"
        );
    }

    #[test]
    fn normalization_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize("  EXAMPLE.com  "), "example.com");
    }

    #[test]
    fn hostnames_follow_rfc_1123() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("a-b.c").is_ok());
        assert!(validate_hostname("api.openai.com.").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-api.openai.com").is_err());
        assert!(validate_hostname("api..com").is_err());
        assert!(validate_hostname("api_openai.com").is_err());
        assert!(validate_hostname("x*.openai.com").is_err());
        let long_label = "a".repeat(64);
        assert!(validate_hostname(&long_label).is_err());
    }

    #[test]
    fn aliases_reject_url_breaking_characters() {
        assert!(validate_alias("my-internal-service").is_ok());
        assert!(validate_alias("vendor.com:8443").is_ok());
        assert!(validate_alias("").is_err());
        assert!(validate_alias(".vendor.com").is_err());
        assert!(validate_alias("vendor.com/evil").is_err());
        assert!(validate_alias("vendor.com:8443:").is_err());
        assert!(validate_alias("a..b").is_err());
    }

    #[test]
    fn aliases_are_strict_ldh_names() {
        assert!(validate_alias("127.0.0.1").is_ok());
        assert!(validate_alias("a-b.c.d").is_ok());
        assert!(validate_alias("vendor.com:1").is_ok());
        assert!(validate_alias("vendor.com:65535").is_ok());
        // Percent escapes, uppercase, control characters and stray separators
        // never reach the wire: the alias becomes `Host` material in slice 2.
        assert!(validate_alias("vendor.com%2Fevil").is_err());
        assert!(validate_alias("Vendor.com").is_err());
        assert!(validate_alias("vendor.com\n").is_err());
        assert!(validate_alias("ven\tdor.com").is_err());
        assert!(validate_alias("vendor.com:0").is_err());
        assert!(validate_alias("vendor.com:abc").is_err());
        assert!(validate_alias("vendor.com:8443:99").is_err());
        assert!(validate_alias("-vendor.com").is_err());
        assert!(validate_alias("ven--dor.com").is_ok());
        assert!(validate_alias("vendor:443").is_ok());
    }

    #[test]
    fn hosts_are_classified() {
        assert_eq!(classify_host("api.openai.com"), super::HostKind::Hostname);
        assert_eq!(classify_host("10.0.1.1"), super::HostKind::IpAddress);
        assert_eq!(classify_host("::1"), super::HostKind::IpAddress);
        assert_eq!(classify_host("not a host"), super::HostKind::Invalid);
    }

    #[test]
    fn creation_accepts_the_exact_derived_alias() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            resolve_creation_alias(&endpoints, Some("api.openai.com")),
            Ok("api.openai.com".to_owned())
        );
    }

    #[test]
    fn creation_rejects_a_differing_explicit_alias_on_a_hostname() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            resolve_creation_alias(&endpoints, Some("openai")),
            Err(AliasRejection::DerivedMismatch {
                derived: "api.openai.com".to_owned()
            })
        );
    }

    #[test]
    fn a_single_ip_literal_derives_the_host() {
        let endpoints = [endpoint(Scheme::Https, "10.0.1.1", 443)];
        assert_eq!(
            resolve_creation_alias(&endpoints, None),
            Ok("10.0.1.1".to_owned())
        );
        let loopback = [endpoint(Scheme::Http, "127.0.0.1", 80)];
        assert_eq!(
            resolve_creation_alias(&loopback, None),
            Ok("127.0.0.1".to_owned())
        );
    }

    #[test]
    fn creation_requires_an_explicit_alias_for_ip_pools() {
        let endpoints = [
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(
            resolve_creation_alias(&endpoints, None),
            Err(AliasRejection::ExplicitRequired(
                NotDerivableReason::IpEndpoints
            ))
        );
        assert_eq!(
            resolve_creation_alias(&endpoints, Some("My-Service")),
            Ok("my-service".to_owned())
        );
    }

    #[test]
    fn update_table_derivable_to_derivable() {
        let current = [endpoint(Scheme::Https, "api.openai.com", 443)];
        let same = [endpoint(Scheme::Https, "API.OpenAI.com.", 443)];
        let moved = [endpoint(Scheme::Https, "api.vendor.com", 443)];
        assert_eq!(
            enforce_update_alias("api.openai.com", &current, &same, None),
            Ok(())
        );
        assert!(matches!(
            enforce_update_alias("api.openai.com", &current, &moved, None),
            Err(AliasRejection::ChangeRejected {
                derived: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn update_table_derivable_to_non_derivable_is_always_rejected() {
        let current = [endpoint(Scheme::Https, "api.openai.com", 443)];
        let ips = [
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        assert!(matches!(
            enforce_update_alias("api.openai.com", &current, &ips, Some("my-service")),
            Err(AliasRejection::ExplicitRequired(_))
        ));
    }

    #[test]
    fn update_table_non_derivable_to_non_derivable() {
        let current = [
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        let next = [
            endpoint(Scheme::Https, "10.0.2.1", 443),
            endpoint(Scheme::Https, "10.0.2.2", 443),
        ];
        assert_eq!(
            enforce_update_alias("my-service", &current, &next, None),
            Ok(())
        );
        assert!(matches!(
            enforce_update_alias("my-service", &current, &next, Some("other-service")),
            Err(AliasRejection::ChangeRejected { .. })
        ));
    }

    #[test]
    fn update_table_non_derivable_to_derivable() {
        let current = [
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        let next = [endpoint(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            enforce_update_alias("api.openai.com", &current, &next, None),
            Ok(())
        );
        assert!(matches!(
            enforce_update_alias("my-service", &current, &next, None),
            Err(AliasRejection::ChangeRejected { .. })
        ));
    }

    #[test]
    fn update_table_no_endpoint_change_tolerates_an_exact_alias() {
        let current = [
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(
            enforce_update_alias("my-service", &current, &current, Some("my-service")),
            Ok(())
        );
        assert!(matches!(
            enforce_update_alias("my-service", &current, &current, Some("renamed")),
            Err(AliasRejection::ChangeRejected { .. })
        ));
    }

    #[test]
    fn reordered_pools_count_as_unchanged() {
        let current = [
            endpoint(Scheme::Https, "a.vendor.com", 443),
            endpoint(Scheme::Https, "b.vendor.com", 443),
        ];
        let reordered = [
            endpoint(Scheme::Https, "b.vendor.com", 443),
            endpoint(Scheme::Https, "a.vendor.com", 443),
        ];
        assert_eq!(
            enforce_update_alias("vendor.com", &current, &reordered, None),
            Ok(())
        );
    }
}
