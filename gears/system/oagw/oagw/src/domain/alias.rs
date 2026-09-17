//! Alias derivation and enforcement (`DESIGN.md` §3.1 "Alias Resolution").
//!
//! Alias behaviour is determined entirely by the endpoint type:
//!
//! | Endpoint type | Rule |
//! |---|---|
//! | Hostname, standard port | auto-derived (`hostname`) |
//! | Hostname, non-standard port | auto-derived (`hostname:port`) |
//! | Multiple hostnames, registrable common suffix | auto-derived (`suffix[:port]`) |
//! | Multiple hostnames, bare public suffix | explicit alias required |
//! | Multiple hostnames, no common suffix | explicit alias required |
//! | IP addresses | explicit alias required |
//!
//! A single hostname is always derivable (no PSL check — the table only
//! PSL-validates the multi-host common suffix); IP endpoints are never
//! derivable.

use psl::domain_str;

use super::error::DomainError;
use super::model::{Endpoint, is_ip_literal, normalize_alias, validate_alias};

/// Result of alias resolution for a create/replace operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasOutcome {
    /// Alias derived from the endpoints.
    Derived(String),
    /// Alias supplied by the operator (endpoints are non-derivable).
    Explicit(String),
}

/// Compute the derived alias for an endpoint pool, or `None` when an explicit
/// alias is required.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let mut hosts: Vec<&str> = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if is_ip_literal(&endpoint.host) {
            return None;
        }
        if !hosts.contains(&endpoint.host.as_str()) {
            hosts.push(endpoint.host.as_str());
        }
    }
    if hosts.is_empty() {
        return None;
    }

    let port = endpoints.first()?.port;
    let scheme = endpoints.first()?.scheme;
    let port_suffix = if port == scheme.standard_port() {
        String::new()
    } else {
        format!(":{port}")
    };

    if hosts.len() == 1 {
        return Some(format!("{}{port_suffix}", hosts[0]));
    }

    // Multiple hostnames: all must share one registrable domain.
    let mut common: Option<&str> = None;
    for host in &hosts {
        let registrable = domain_str(host)?;
        match common {
            None => common = Some(registrable),
            Some(prev) if prev == registrable => {}
            Some(_) => return None,
        }
    }
    common.map(|suffix| format!("{suffix}{port_suffix}"))
}

/// Whether the endpoint pool has a derivable alias.
#[must_use]
pub fn is_derivable(endpoints: &[Endpoint]) -> bool {
    compute_derived_alias(endpoints).is_some()
}

/// Registrable domain suffix of a single hostname, or `None` when the host is
/// an IP literal or has no registrable domain (a bare public suffix).
#[must_use]
pub fn registrable_suffix(host: &str) -> Option<&str> {
    if is_ip_literal(host) {
        return None;
    }
    domain_str(host)
}

/// Derive or take the explicit alias for a new upstream.
///
/// # Errors
///
/// Returns a validation error when the requested alias is malformed, when a
/// derivable pool is given a different explicit alias, or when a
/// non-derivable pool omits the alias.
pub fn derive_alias(
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> Result<AliasOutcome, DomainError> {
    let derived = compute_derived_alias(endpoints);
    match (derived, requested) {
        (Some(derived), None) => Ok(AliasOutcome::Derived(derived)),
        (Some(derived), Some(requested)) => {
            let normalized = normalize_alias(requested);
            validate_alias(&normalized)?;
            if normalized == derived {
                // Exact match with the derived value: tolerated as an
                // idempotent no-op.
                Ok(AliasOutcome::Derived(derived))
            } else {
                Err(DomainError::validation(format!(
                    "alias `{normalized}` does not match the derived alias `{derived}`: \
                     hostname-based endpoints auto-derive the alias"
                )))
            }
        }
        (None, None) => Err(DomainError::validation(
            "explicit alias required: endpoints are IP-based or have no registrable common \
             suffix",
        )),
        (None, Some(requested)) => {
            let normalized = normalize_alias(requested);
            validate_alias(&normalized)?;
            Ok(AliasOutcome::Explicit(normalized))
        }
    }
}

/// Enforce alias immutability on a replace operation.
///
/// # Errors
///
/// Returns a validation error when the operation would change the alias in
/// any way; the operator must delete and re-create the upstream instead.
pub fn enforce_alias_update(
    existing_alias: &str,
    existing_derivable: bool,
    new_endpoints: &[Endpoint],
    requested: Option<&str>,
) -> Result<(), DomainError> {
    if let Some(requested) = requested {
        let normalized = normalize_alias(requested);
        validate_alias(&normalized)?;
        if normalized != existing_alias {
            return Err(DomainError::validation(format!(
                "alias is immutable once set: `{existing_alias}` cannot become `{normalized}`; \
                 delete and re-create the upstream instead"
            )));
        }
    }

    match (compute_derived_alias(new_endpoints), existing_derivable) {
        (Some(derived), _) if derived == existing_alias => Ok(()),
        (Some(derived), _) => Err(DomainError::validation(format!(
            "alias is immutable once set: endpoints would change the alias from \
             `{existing_alias}` to `{derived}`; delete and re-create the upstream"
        ))),
        (None, true) => Err(DomainError::validation(
            "alias is immutable once set: endpoints changed from derivable to non-derivable; \
             delete and re-create the upstream",
        )),
        (None, false) => Ok(()),
    }
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod alias_tests;
