//! Target endpoint selection (DESIGN §3.1 "Multi-Endpoint Load Balancing",
//! ADR 0001 Appendix A "X-OAGW-Target-Host Behavior Matrix").
//!
//! The pool of one upstream is a load-balancing group. Which endpoint serves a
//! request is decided by:
//!
//! | Pool | Alias source | `X-OAGW-Target-Host` |
//! |---|---|---|
//! | one endpoint | any | optional — the only endpoint is used |
//! | several endpoints | explicit | optional — round-robin |
//! | several endpoints | common suffix | **required** → `400 MissingTargetHost` |
//!
//! A header value that is malformed answers `400 InvalidTargetHost`; a
//! well-formed value that matches no configured endpoint answers
//! `400 UnknownTargetHost`.

use crate::domain::alias::{self, AliasSource};
use crate::domain::error::DomainError;
use crate::domain::models::{Endpoint, Upstream};

/// Inbound header carrying the caller's endpoint choice.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// How the endpoint that served a request was picked (DESIGN §4.2
/// `oagw_routing_endpoint_selected{selection_method}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// The caller pinned the endpoint with `X-OAGW-Target-Host`.
    ExplicitHeader,
    /// The endpoint was chosen by the pool's round-robin cursor.
    RoundRobin,
    /// The pool holds a single endpoint.
    Default,
}

/// The endpoint a request must be forwarded to.
#[derive(Debug)]
pub struct TargetEndpoint<'a> {
    /// Endpoint to contact.
    pub endpoint: &'a Endpoint,
    /// How it was selected, for the routing metrics.
    pub method: SelectionMethod,
}

/// Selects the endpoint of `upstream` that must serve the request.
///
/// `cursor` is the caller's round-robin counter; it is *not* advanced here —
/// the caller advances it after a successful selection so a rejected request
/// does not consume a slot.
///
/// # Errors
///
/// Returns [`DomainError::MissingTargetHost`],
/// [`DomainError::InvalidTargetHost`] or [`DomainError::UnknownTargetHost`]
/// per the ADR 0001 matrix, and [`DomainError::LinkUnavailable`] when the pool
/// is empty.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    header_host: Option<&str>,
    cursor: u64,
) -> Result<TargetEndpoint<'a>, DomainError> {
    let endpoints = &upstream.server.endpoints;
    let endpoints = non_empty(endpoints, upstream)?;
    let hosts: Vec<String> = endpoints
        .iter()
        .map(|endpoint| alias::normalize_alias(&endpoint.host))
        .collect();

    if let Some(raw) = header_host.map(str::trim).filter(|raw| !raw.is_empty()) {
        let wanted = alias::normalize_alias(raw);
        // Review evidence (privilege boundary — routing key validation):
        // * Guardrail: ADR 0001 — the header must be a bare hostname or IP,
        //   never a port, a path or arbitrary text, so it can never smuggle a
        //   scheme, a port or a path component into the outbound URL.
        // * Rationale: the value is spliced into the outbound request target
        //   verbatim; accepting anything else would let a caller redirect
        //   traffic to an arbitrary socket.
        // * Validation performed: the value is re-validated with the same
        //   hostname rules the control plane applies to endpoint hosts and
        //   must additionally match a configured endpoint of this upstream.
        if is_malformed_target_host(&wanted) {
            return Err(DomainError::InvalidTargetHost {
                invalid_value: raw.to_owned(),
                upstream_id: Some(upstream.id),
            });
        }
        return match hosts.iter().position(|host| host == &wanted) {
            Some(index) => Ok(TargetEndpoint {
                endpoint: &endpoints[index],
                method: SelectionMethod::ExplicitHeader,
            }),
            None => Err(DomainError::UnknownTargetHost {
                invalid_value: wanted,
                upstream_id: Some(upstream.id),
                valid_hosts: hosts,
            }),
        };
    }

    if endpoints.len() == 1 {
        return Ok(TargetEndpoint {
            endpoint: &endpoints[0],
            method: SelectionMethod::Default,
        });
    }

    // Several endpoints with an operator-supplied alias: the alias carries no
    // host information, so the pool load-balances on its own.
    if alias_source_of(upstream) == AliasSource::Explicit {
        let index = usize::try_from(cursor % endpoints.len() as u64).unwrap_or(0);
        return Ok(TargetEndpoint {
            endpoint: &endpoints[index],
            method: SelectionMethod::RoundRobin,
        });
    }

    Err(DomainError::MissingTargetHost {
        upstream_id: Some(upstream.id),
        path: None,
        valid_hosts: hosts,
    })
}

/// Whether a target-host header value can be spliced into an outbound URL.
///
/// ADR 0001 only allows a bare hostname or IP literal: no port, no path, no
/// IPv6 brackets. `normalize_host` tolerates a `host:port` spelling for single
/// endpoint configurations, so the port separator is rejected explicitly here.
fn is_malformed_target_host(value: &str) -> bool {
    value.is_empty()
        || value.contains([':', '/', '?', '#', '[', ']', '@', '%'])
        || crate::domain::alias::validate_hostname(value).is_err()
}

/// Source of the alias of `upstream`, used to decide whether the pool may
/// load-balance without an explicit target host.
#[must_use]
pub fn alias_source_of(upstream: &Upstream) -> AliasSource {
    match crate::domain::alias::derive_alias(&upstream.server.endpoints) {
        Ok(derivation) => derivation.source,
        Err(_) => AliasSource::Explicit,
    }
}

fn non_empty<'a>(
    endpoints: &'a [Endpoint],
    upstream: &Upstream,
) -> Result<&'a [Endpoint], DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::LinkUnavailable {
            upstream_id: Some(upstream.id),
            host: None,
        });
    }
    Ok(endpoints)
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
