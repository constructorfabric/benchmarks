//! Alias derivation and validation (DESIGN §3.1 "Alias Derivation", PRD §5.5).
//!
//! An upstream alias is the routing key a client uses in the proxy URL. It is
//! either *derived* from the endpoint pool or supplied explicitly (mandatory
//! for IP-based pools). The rules implemented here are:
//!
//! | endpoint shape                        | derived alias                 |
//! |---------------------------------------|-------------------------------|
//! | one hostname, standard port           | `hostname`                    |
//! | one hostname, non-standard port       | `hostname:port`               |
//! | several hostnames, shared suffix      | longest common registrable suffix (port preserved when non-standard) |
//! | several hostnames, no shared suffix   | not derivable → 400           |
//! | any IP literal                        | not derivable → explicit alias required |
//!
//! Aliases are always normalised (lowercase, trailing dot stripped) and must
//! match the upstream schema pattern
//! `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.

use std::net::IpAddr;

use crate::domain::error::DomainError;
use crate::domain::models::Endpoint;

/// Maximum length of an RFC 1123 hostname (RFC 1035 §2.3.4).
pub const MAX_HOSTNAME_LEN: usize = 253;

/// Maximum length of a single DNS label (RFC 1035 §2.3.4).
pub const MAX_LABEL_LEN: usize = 63;

/// How the alias of an upstream was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasSource {
    /// Derived from a single endpoint host.
    SingleHost,
    /// Derived from the longest common registrable suffix of a pool.
    CommonSuffix,
    /// Supplied by the operator (IP-based or non-derivable pool).
    Explicit,
}

/// Result of alias derivation for an endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasDerivation {
    /// Normalised alias.
    pub alias: String,
    /// How the alias was established.
    pub source: AliasSource,
    /// Common suffix, when the pool was collapsed into one suffix.
    pub common_suffix: Option<String>,
    /// Port preserved in the alias, when non-standard for the scheme.
    pub port: Option<u16>,
}

/// Why an alias could not be derived.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    /// The pool contains an IP literal; an explicit alias is required.
    #[error("endpoint host '{host}' is an IP address; an explicit alias is required")]
    IpBased {
        /// Offending host.
        host: String,
    },
    /// The pool's hosts share no registrable suffix.
    #[error("{reason}")]
    NotDerivable {
        /// Human-readable reason.
        reason: String,
        /// Normalised hosts that were considered.
        valid_hosts: Vec<String>,
    },
    /// An endpoint host is not a valid hostname or IP literal.
    #[error("{reason}")]
    InvalidEndpoint {
        /// Human-readable reason.
        reason: String,
    },
}

/// Normalises an alias or hostname: trimmed, lowercased, trailing dots and
/// whitespace removed.
#[must_use]
pub fn normalize_alias(raw: &str) -> String {
    raw.trim()
        .trim_end_matches('.')
        .trim()
        .to_ascii_lowercase()
}

/// Whether `raw` is a valid normalised alias (upstream schema pattern).
#[must_use]
pub fn alias_is_valid(raw: &str) -> bool {
    let alias = normalize_alias(raw);
    if alias.is_empty() || alias.len() > MAX_HOSTNAME_LEN {
        return false;
    }
    let Some(first) = alias.chars().next() else {
        return false;
    };
    let Some(last) = alias.chars().last() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return false;
    }
    alias.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == ':' || c == '-'
    })
}

/// Validates an RFC 1123 hostname.
///
/// # Errors
///
/// Returns a [`DomainError::ValidationError`] describing the first rule the
/// hostname violates.
pub fn validate_hostname(raw: &str) -> Result<(), DomainError> {
    let host = normalize_alias(raw);
    if host.is_empty() {
        return Err(DomainError::validation_with_value(
            "hostname must not be empty",
            raw.to_owned(),
        ));
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(DomainError::validation_with_value(
            "hostname exceeds 253 characters",
            raw.to_owned(),
        ));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(DomainError::validation_with_value(
                "hostname must not contain empty labels",
                raw.to_owned(),
            ));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(DomainError::validation_with_value(
                "hostname label exceeds 63 characters",
                raw.to_owned(),
            ));
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(DomainError::validation_with_value(
                "hostname labels may only contain ASCII letters, digits and hyphens",
                raw.to_owned(),
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainError::validation_with_value(
                "hostname labels must not start or end with a hyphen",
                raw.to_owned(),
            ));
        }
    }
    Ok(())
}

/// Normalises an endpoint host and validates it as a hostname or IP literal.
///
/// # Errors
///
/// Returns a [`DomainError::ValidationError`] when the host is empty, carries
/// brackets, a port or a path, or is not an RFC 1123 hostname / IP literal.
pub fn normalize_host(raw: &str) -> Result<String, DomainError> {
    let host = normalize_alias(raw);
    if host.is_empty() {
        return Err(DomainError::validation_with_value(
            "endpoint host must not be empty",
            raw.to_owned(),
        ));
    }
    if host.starts_with('[') || host.ends_with(']') || host.contains('[') || host.contains(']') {
        return Err(DomainError::validation_with_value(
            "IPv6 endpoint hosts must be written without brackets",
            raw.to_owned(),
        ));
    }
    if host.contains('/') || host.contains('?') || host.contains('#') {
        return Err(DomainError::validation_with_value(
            "endpoint host must not contain path or query characters",
            raw.to_owned(),
        ));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(host);
    }
    // Strip an optional port so that `host:port` spellings of a single
    // endpoint remain readable; the port is carried by `Endpoint.port`.
    let candidate = host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h);
    validate_hostname(candidate)?;
    Ok(host)
}

/// Validates the host of a single endpoint.
///
/// # Errors
///
/// Returns a [`DomainError::ValidationError`] when the host is neither an
/// RFC 1123 hostname nor an IP literal.
pub fn validate_endpoint_host(endpoint: &Endpoint) -> Result<(), DomainError> {
    normalize_host(&endpoint.host).map(|_| ())
}

/// Derives the alias of an upstream from its endpoint pool.
///
/// # Errors
///
/// Returns [`AliasError`] when the pool is empty, when a host is invalid, when
/// a host is an IP literal (explicit alias required) or when the pool has no
/// common registrable suffix.
pub fn derive_alias(endpoints: &[Endpoint]) -> Result<AliasDerivation, AliasError> {
    derive_alias_with(endpoints, psl::domain_str)
}

/// Derives the alias using an injectable public-suffix resolver.
///
/// `registrable_domain` receives a normalised host and returns its
/// registrable domain, or `None` when the host has none (single label or bare
/// public suffix).
pub fn derive_alias_with<F>(
    endpoints: &[Endpoint],
    registrable_domain: F,
) -> Result<AliasDerivation, AliasError>
where
    F: Fn(&str) -> Option<&str>,
{
    if endpoints.is_empty() {
        return Err(AliasError::NotDerivable {
            reason: "upstream has no endpoints".to_owned(),
            valid_hosts: Vec::new(),
        });
    }

    let mut hosts = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        match normalize_host(&endpoint.host) {
            Ok(host) => hosts.push(host),
            Err(err) => {
                return Err(AliasError::InvalidEndpoint {
                    reason: err.detail(),
                });
            }
        }
    }

    let port = endpoints
        .first()
        .filter(|endpoint| !endpoint.scheme.is_standard_port(endpoint.port))
        .map(|endpoint| endpoint.port);

    let first = &hosts[0];
    if first.parse::<IpAddr>().is_ok() {
        return Err(AliasError::IpBased {
            host: first.clone(),
        });
    }

    if hosts.len() == 1 {
        let alias = match port {
            Some(p) => format!("{first}:{p}"),
            None => first.clone(),
        };
        if !alias_is_valid(&alias) {
            return Err(AliasError::InvalidEndpoint {
                reason: format!("derived alias '{alias}' is not a valid alias"),
            });
        }
        return Ok(AliasDerivation {
            alias,
            source: AliasSource::SingleHost,
            common_suffix: registrable_domain(first).map(str::to_owned),
            port,
        });
    }

    let common = common_suffix(&hosts).ok_or_else(|| AliasError::NotDerivable {
        reason: format!(
            "upstream endpoints {} share no common domain suffix",
            hosts.join(", ")
        ),
        valid_hosts: hosts.clone(),
    })?;

    if common.split('.').count() < 2 {
        return Err(AliasError::NotDerivable {
            reason: format!("common suffix '{common}' is a bare public suffix"),
            valid_hosts: hosts.clone(),
        });
    }
    if registrable_domain(&common).is_none() {
        return Err(AliasError::NotDerivable {
            reason: format!("common suffix '{common}' is not a registrable domain"),
            valid_hosts: hosts.clone(),
        });
    }

    let alias = match port {
        Some(p) => format!("{common}:{p}"),
        None => common.clone(),
    };
    Ok(AliasDerivation {
        alias,
        source: AliasSource::CommonSuffix,
        common_suffix: Some(common),
        port,
    })
}

/// Longest label-wise common suffix of `hosts`, or `None` when they share no
/// suffix.
///
/// The comparison is ASCII case-insensitive and the returned suffix is
/// normalised to lowercase.
#[must_use]
pub fn common_suffix(hosts: &[String]) -> Option<String> {
    let mut iter = hosts.iter();
    let first = iter.next()?;
    let mut labels: Vec<String> = first
        .split('.')
        .rev()
        .map(str::to_ascii_lowercase)
        .collect();
    for host in iter {
        let candidate: Vec<&str> = host.split('.').rev().collect();
        let shared = labels
            .iter()
            .zip(candidate.iter())
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
            .count();
        labels.truncate(shared);
        if labels.is_empty() {
            return None;
        }
    }
    if labels.is_empty() {
        return None;
    }
    labels.reverse();
    Some(labels.join("."))
}

/// Enforces the alias rules when an upstream is replaced.
///
/// * Hostname-derived aliases are immutable: the alias must keep matching the
///   derived value whenever the endpoint pool changes.
/// * IP-based upstreams are the only ones allowed to change their explicit
///   alias (DESIGN "PUT (Replace) — Upstream").
///
/// # Errors
///
/// Returns [`DomainError::AliasMismatch`] when the supplied alias differs from
/// the newly derived one, [`DomainError::AliasNotDerivable`] when a
/// hostname-derived upstream is replaced by a non-derivable pool, and
/// [`DomainError::ValidationError`] when the alias is malformed.
pub fn enforce_alias_update(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    requested_alias: Option<&str>,
    requested_endpoints: &[Endpoint],
) -> Result<(), DomainError> {
    enforce_alias_update_with(
        existing_alias,
        existing_endpoints,
        requested_alias,
        requested_endpoints,
        |hosts| derive_alias(hosts).map_err(AliasError::into_domain),
    )
}

/// [`enforce_alias_update`] with an injectable derivation function (used by
/// tests and by callers that resolve the public-suffix list themselves).
///
/// # Errors
///
/// Propagates [`DomainError::ValidationError`] as produced by `derive`.
pub fn enforce_alias_update_with<D>(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    requested_alias: Option<&str>,
    requested_endpoints: &[Endpoint],
    derive: D,
) -> Result<(), DomainError>
where
    D: Fn(&[Endpoint]) -> Result<AliasDerivation, DomainError>,
{
    let existing = normalize_alias(existing_alias);
    if !alias_is_valid(&existing) {
        return Err(DomainError::validation_with_value(
            "stored alias is not a valid alias",
            existing,
        ));
    }

    let pool_unchanged = endpoints_equal(existing_endpoints, requested_endpoints);
    let requested_normalized = requested_alias.map(normalize_alias);

    // An upstream whose pool cannot produce a derivation holds an
    // operator-supplied (explicit) alias, which may be changed freely.
    let existing_is_explicit = derive(existing_endpoints).is_err();

    if pool_unchanged {
        if let Some(alias) = requested_normalized
            && alias != existing
            && !existing_is_explicit
        {
            return Err(DomainError::validation_with_value(
                "alias is immutable for hostname-derived upstreams",
                alias,
            ));
        }
        return Ok(());
    }

    // The pool changed: recompute the alias.
    match derive(requested_endpoints) {
        Ok(derivation) => {
            if derivation.alias == existing {
                return Ok(());
            }
            if existing_is_explicit {
                // The pool moved from IP literals to hostnames: the alias must
                // follow the new derivation.
                return Err(DomainError::validation_with_value(
                    format!(
                        "alias '{}' does not match the alias derived from the new endpoint pool '{}'",
                        existing, derivation.alias
                    ),
                    derivation.alias,
                ));
            }
            Err(DomainError::AliasMismatch {
                detail: format!(
                    "alias '{}' does not match the alias derived from the new endpoint pool '{}'",
                    existing, derivation.alias
                ),
                provided: existing,
                derived: derivation.alias,
            })
        }
        // The new pool is IP-based or has no common suffix: it can only be
        // reached through an explicit alias, which is allowed to change when
        // the existing upstream already held one.
        Err(err) if existing_is_explicit => {
            let _ = err;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Structural comparison of two endpoint pools (order-insensitive).
#[must_use]
pub fn endpoints_equal(a: &[Endpoint], b: &[Endpoint]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut left: Vec<&Endpoint> = a.iter().collect();
    let mut right: Vec<&Endpoint> = b.iter().collect();
    let key = |endpoint: &Endpoint| {
        (
            normalize_alias(&endpoint.host),
            endpoint.scheme.as_str(),
            endpoint.port,
        )
    };
    left.sort_by_key(|e| key(e));
    right.sort_by_key(|e| key(e));
    left.iter()
        .zip(right.iter())
        .all(|(l, r)| key(l) == key(r))
}

impl AliasError {
    /// Converts the derivation failure into the wire-visible domain error.
    #[must_use]
    pub fn into_domain(self) -> DomainError {
        match self {
            Self::IpBased { host } => DomainError::AliasNotDerivable {
                detail: format!(
                    "endpoint host '{host}' is an IP address; an explicit alias is required"
                ),
                valid_hosts: vec![host],
            },
            Self::NotDerivable { reason, valid_hosts } => {
                DomainError::AliasNotDerivable { detail: reason, valid_hosts }
            }
            Self::InvalidEndpoint { reason } => DomainError::validation(reason),
        }
    }
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod tests;
