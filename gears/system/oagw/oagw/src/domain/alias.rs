//! Alias derivation, normalisation and update enforcement.
//!
//! Aliases are the routing key in `/oagw/v1/proxy/{alias}/...`, so their
//! behaviour is fixed by DESIGN.md §3.3: derived from hostname endpoints,
//! explicit for IP or non-derivable pools, immutable once set.

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, ServerConfig};

/// Outcome of deriving an alias from an endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedAlias {
    /// Derivation succeeded.
    Derived(String),
    /// Derivation is not possible; an explicit alias is required.
    NotDerivable(String),
}

/// Normalises an alias: ASCII lowercase with trailing dots stripped.
#[must_use]
pub fn normalise(alias: &str) -> String {
    let trimmed = alias.trim();
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    trimmed.to_ascii_lowercase()
}

/// Whether the alias matches the accepted pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-'))
}

/// The registrable common suffix of a set of hosts, when it has at least two
/// labels and is not itself a bare public suffix.
#[must_use]
pub fn common_domain_suffix(hosts: &[&str]) -> Option<String> {
    let reversed: Vec<Vec<&str>> = hosts
        .iter()
        .map(|host| host.split('.').rev().collect::<Vec<_>>())
        .collect();
    let shortest = reversed.iter().map(Vec::len).min()?;
    let mut shared = 0;
    while shared < shortest {
        let label = reversed[0][shared];
        if reversed.iter().all(|labels| labels[shared] == label) {
            shared += 1;
        } else {
            break;
        }
    }
    if shared < 2 {
        return None;
    }
    let mut labels: Vec<&str> = reversed[0][..shared].to_vec();
    labels.reverse();
    let candidate = labels.join(".");
    // A bare public suffix (`co.uk`) is not a registrable domain, so it can
    // never name a pool. A deeper shared suffix (`api.example.com`) is a
    // legitimate one: it sits under a registrable domain.
    psl::domain_str(&candidate)?;
    Some(candidate)
}

/// Derives the alias for an endpoint pool.
#[must_use]
pub fn derive(endpoints: &[Endpoint]) -> DerivedAlias {
    if endpoints.is_empty() {
        return DerivedAlias::NotDerivable("no endpoints configured".to_owned());
    }
    let port = endpoints[0].port;
    let scheme = endpoints[0].scheme;
    let port_suffix = if scheme.is_standard_port(port) {
        String::new()
    } else {
        format!(":{port}")
    };

    if endpoints.len() == 1 {
        let endpoint = &endpoints[0];
        let host = endpoint.normalised_host();
        if host.parse::<std::net::IpAddr>().is_ok() {
            return DerivedAlias::NotDerivable(format!(
                "explicit alias required for IP endpoint {host}"
            ));
        }
        return DerivedAlias::Derived(format!("{host}{port_suffix}"));
    }

    let hosts: Vec<String> = endpoints.iter().map(Endpoint::normalised_host).collect();
    if hosts
        .iter()
        .any(|host| host.parse::<std::net::IpAddr>().is_ok())
    {
        return DerivedAlias::NotDerivable("explicit alias required for IP endpoints".to_owned());
    }
    let refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
    match common_domain_suffix(&refs) {
        Some(suffix) => DerivedAlias::Derived(format!("{suffix}{port_suffix}")),
        None => DerivedAlias::NotDerivable(
            "explicit alias required: endpoints share no registrable common suffix".to_owned(),
        ),
    }
}

/// Whether the endpoints are all IP literals.
#[must_use]
pub fn is_ip_based(endpoints: &[Endpoint]) -> bool {
    !endpoints.is_empty()
        && endpoints.iter().all(|endpoint| {
            endpoint
                .normalised_host()
                .parse::<std::net::IpAddr>()
                .is_ok()
        })
}

/// Resolves the alias for a create operation.
///
/// # Errors
///
/// Returns [`DomainError::Invalid`] when the supplied alias contradicts the
/// derived one, or when no alias can be supplied or derived.
pub fn resolve_for_create(
    server: &ServerConfig,
    alias: Option<&str>,
) -> Result<String, DomainError> {
    match derive(&server.endpoints) {
        DerivedAlias::Derived(derived) => match alias.map(normalise) {
            Some(supplied) if supplied == derived => Ok(derived),
            Some(supplied) => Err(DomainError::Invalid(format!(
                "alias '{supplied}' does not match the derived alias '{derived}'; \
                 aliases for hostname endpoints are derived automatically"
            ))),
            None => Ok(derived),
        },
        DerivedAlias::NotDerivable(reason) => match alias.map(normalise) {
            Some(supplied) if is_valid(&supplied) => Ok(supplied),
            Some(supplied) => Err(DomainError::Invalid(format!(
                "alias '{supplied}' is not a valid alias"
            ))),
            None => Err(DomainError::Invalid(reason)),
        },
    }
}

/// Enforces the alias transition rules for a replace operation.
///
/// # Errors
///
/// Returns [`DomainError::Invalid`] when the endpoints would change the
/// derived alias, or when an alias change is requested.
pub fn enforce_alias_update(
    current_alias: &str,
    server: &ServerConfig,
    alias: Option<&str>,
) -> Result<String, DomainError> {
    if alias.is_some_and(|value| normalise(value) != current_alias) {
        return Err(DomainError::Invalid(
            "alias is immutable; delete and re-create the upstream to change it".to_owned(),
        ));
    }
    match derive(&server.endpoints) {
        DerivedAlias::Derived(derived) if derived == current_alias => Ok(current_alias.to_owned()),
        DerivedAlias::Derived(derived) => Err(DomainError::Invalid(format!(
            "endpoint change would alter the derived alias from '{current_alias}' to \
             '{derived}'; delete and re-create the upstream instead"
        ))),
        // A pool that cannot derive an alias is legitimate when it was
        // already explicit: the caller left the alias alone (`None`), so
        // nothing drifts. Re-asserting an alias the new pool cannot produce
        // is a rename in disguise and is refused.
        DerivedAlias::NotDerivable(_) if alias.is_none() => Ok(current_alias.to_owned()),
        DerivedAlias::NotDerivable(_) => Err(DomainError::Invalid(
            "endpoint change would make the alias non-derivable; delete and re-create \
             the upstream instead"
                .to_owned(),
        )),
    }
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod tests;
