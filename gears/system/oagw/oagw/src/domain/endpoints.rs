//! Endpoint selection and target-host resolution
//! (`cpt-cf-oagw-flow-request-proxy-target-host-selection`,
//! `cpt-cf-oagw-algo-request-proxy-endpoint-select`).
//!
//! The matrix the DoD fixes, as a function of the endpoint pool's alias shape
//! and the presence of `x-oagw-target-host`:
//!
//! | pool | header absent | header present |
//! |---|---|---|
//! | one endpoint | that endpoint | validated against the endpoint host |
//! | several, explicit alias | round-robin | validated, then that endpoint |
//! | several, common-suffix alias | **`400` missing target host** | validated, then that endpoint |
//!
//! A supplied value is always a bare hostname or IP address with no port, path
//! or special character, and must name a configured endpoint host.
//!
//! The round-robin cursor is *Data Plane state*, not a value here: this module
//! receives the index to use, so the cursor's atomicity and its re-derivation
//! when a pool changes stay with the state owner.

use crate::domain::alias::{self, alias_class, AliasClass};
use crate::domain::dto::{Endpoint, EndpointScheme, ServerConfig};
use crate::domain::error::DomainError;

/// The alias form of an endpoint pool (`inst-rp-target-2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasShape {
    /// Exactly one endpoint.
    Single,
    /// Several endpoints whose alias is the operator's explicit choice.
    Explicit,
    /// Several endpoints whose alias is the registrable common suffix the
    /// derivation computes, which makes the header mandatory.
    CommonSuffix,
}

/// Classify a pool by its alias shape.
#[must_use]
pub fn alias_shape(pool: &ServerConfig, alias: &str) -> AliasShape {
    match pool.endpoints.len() {
        0 | 1 => AliasShape::Single,
        _ => match alias_class(pool) {
            AliasClass::Derivable => {
                let derived = alias::try_derive_alias(pool)
                    .ok()
                    .flatten()
                    .map(|derived| alias::normalize_alias(&derived))
                    .unwrap_or_default();
                if alias::normalize_alias(alias) == derived {
                    AliasShape::CommonSuffix
                } else {
                    AliasShape::Explicit
                }
            }
            AliasClass::NonDerivable => AliasShape::Explicit,
        },
    }
}

/// How the endpoint was chosen (`inst-rp-target-15`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// The caller named the endpoint through `x-oagw-target-host`.
    ExplicitHeader,
    /// The pool rotated to the next endpoint.
    RoundRobin,
    /// The pool has one endpoint, so there was nothing to choose.
    Default,
}

impl SelectionMethod {
    /// The value the pipeline boundary records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// The selected endpoint and how it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedEndpoint {
    /// The endpoint to connect to.
    pub endpoint: Endpoint,
    /// The selection method.
    pub method: SelectionMethod,
}

/// The valid endpoint hosts of a pool, for the error's `valid_hosts` field.
#[must_use]
pub fn valid_hosts(pool: &ServerConfig) -> Vec<String> {
    let mut hosts: Vec<String> = pool.endpoints.iter().map(|endpoint| endpoint.host.clone()).collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Confirm the pool is uniform in scheme and port
/// (`inst-rp-al-endpoint-1`).
///
/// Upstream configuration validation guarantees this; the check is repeated
/// here because the data plane reads stored records, not validated payloads.
///
/// # Errors
///
/// Returns a validation error naming `server` when the pool is not uniform.
pub fn require_uniform_pool(pool: &ServerConfig) -> Result<(), DomainError> {
    let Some(first) = pool.endpoints.first() else {
        return Err(DomainError::field_rejection(
            "server",
            "the endpoint pool carries no endpoint",
        ));
    };
    let uniform = pool
        .endpoints
        .iter()
        .all(|endpoint| endpoint.scheme == first.scheme && endpoint.port == first.port);
    if uniform {
        Ok(())
    } else {
        Err(DomainError::field_rejection(
            "server",
            "the endpoint pool must be uniform in scheme and port",
        ))
    }
}

/// Whether a target-host value is a bare hostname or IP address
/// (`inst-rp-target-10`).
///
/// A port, a path, userinfo or any special character disqualifies the value.
#[must_use]
pub fn target_host_is_valid(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    if value.chars().any(|c| {
        c.is_control() || c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@' | ':' | '%' | '\\' | '[' | ']')
    }) {
        return false;
    }
    crate::domain::validation::host_is_ip(value)
        || crate::domain::validation::validate_host("x-oagw-target-host", value).is_ok()
}

/// Select the endpoint for a proxy request
/// (`inst-rp-target-3` .. `-13`, `inst-rp-al-endpoint-2` .. `-7`).
///
/// `next_index` is the round-robin cursor read: it receives the pool length
/// and returns the index to select. The cursor itself is Data Plane state.
///
/// # Errors
///
/// Returns the missing-, invalid- or unknown-target-host error of the
/// behaviour matrix, or a validation error for a non-uniform pool or a scheme
/// the allowlist does not admit.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-1
// `inst-rp-target-1` .. `-15`, `inst-rp-al-endpoint-1` .. `-15`: target-host
// resolution and endpoint selection — the `x-oagw-target-host` routing header,
// the pool allowlist, the scheme admission of the gear configuration, and the
// selection callback the cursor state drives.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-11
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-12
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-13
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-14
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-15
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-9
pub fn select_endpoint(
    pool: &ServerConfig,
    upstream_id: Option<String>,
    alias: &str,
    target_host: Option<&str>,
    allow_http_upstream: bool,
    trace_id: Option<String>,
    next_index: impl FnOnce(usize) -> usize,
) -> Result<SelectedEndpoint, DomainError> {
    require_uniform_pool(pool)?;

    // A supplied value is always validated, whatever the alias shape is.
    if let Some(value) = target_host.map(str::trim) {
        if !target_host_is_valid(value) {
            return Err(DomainError::InvalidTargetHost {
                upstream_id,
                invalid_value: crate::domain::validation::bound_invalid_value(value),
                trace_id,
            });
        }
        let normalized = value.strip_suffix('.').unwrap_or(value).to_ascii_lowercase();
        let Some(endpoint) = pool
            .endpoints
            .iter()
            .find(|endpoint| endpoint.host.eq_ignore_ascii_case(&normalized))
            .cloned()
        else {
            return Err(DomainError::UnknownTargetHost {
                upstream_id,
                invalid_value: crate::domain::merge::bound_target_host_echo(value),
                valid_hosts: valid_hosts(pool),
                trace_id,
            });
        };
        return admit(endpoint, SelectionMethod::ExplicitHeader, allow_http_upstream, trace_id);
    }

    match alias_shape(pool, alias) {
        AliasShape::Single => {
            let Some(endpoint) = pool.endpoints.first().cloned() else {
                return Err(DomainError::field_rejection(
                    "server",
                    "the endpoint pool carries no endpoint",
                ));
            };
            admit(endpoint, SelectionMethod::Default, allow_http_upstream, trace_id)
        }
        AliasShape::Explicit => {
            let index = next_index(pool.endpoints.len());
            let endpoint = pool.endpoints[index % pool.endpoints.len()].clone();
            admit(endpoint, SelectionMethod::RoundRobin, allow_http_upstream, trace_id)
        }
        AliasShape::CommonSuffix => Err(DomainError::MissingTargetHost {
            upstream_id,
            alias: Some(alias::normalize_alias(alias)),
            valid_hosts: valid_hosts(pool),
            trace_id,
        }),
    }
}
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-15
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-14
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-13
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-12
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-11
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-10
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-target-host-selection:p1:inst-rp-target-1

/// Enforce the endpoint scheme allowlist before any connection is opened
/// (`inst-rp-al-endpoint-12` .. `-14`).
///
/// `https` and `wss` are always legal, `wt` is a legal endpoint scheme value,
/// and `http` is legal exactly while `allow_http_upstream` is true.
///
/// # Errors
///
/// Returns a validation error naming the scheme when it is not admitted.
pub fn admit(
    endpoint: Endpoint,
    method: SelectionMethod,
    allow_http_upstream: bool,
    trace_id: Option<String>,
) -> Result<SelectedEndpoint, DomainError> {
    if !endpoint.scheme.is_allowed(allow_http_upstream) {
        return Err(DomainError::ValidationError {
            detail: format!(
                "the endpoint scheme `{}` is not admitted by the allowlist",
                scheme_name(endpoint.scheme)
            ),
            path: Some("server.endpoints.scheme".to_owned()),
            trace_id,
        });
    }
    Ok(SelectedEndpoint { endpoint, method })
}

/// The wire name of an endpoint scheme.
#[must_use]
pub const fn scheme_name(scheme: EndpointScheme) -> &'static str {
    match scheme {
        EndpointScheme::Http => "http",
        EndpointScheme::Https => "https",
        EndpointScheme::Wss => "wss",
        EndpointScheme::Wt => "wt",
        EndpointScheme::Grpc => "grpc",
    }
}
