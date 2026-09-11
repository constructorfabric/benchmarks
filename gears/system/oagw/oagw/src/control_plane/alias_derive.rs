//! Alias derivation — `cpt-cf-oagw-algo-alias-derive`.
//!
//! Derives the routing alias an endpoint set resolves to, reconciles it with a
//! caller-supplied one, and confirms alias immutability across a replacement.
//! Normalization itself is `cpt-cf-oagw-algo-alias-normalize`, which lives on
//! the [`Hostname`], [`Alias`], and [`EndpointHost`] value objects; this module
//! only ever holds a value produced by one of those constructors, so
//! derivation and later resolution cannot disagree about the shape of a value.
//!
//! No detail produced here carries a host, an alias value, or any other
//! configuration value.

// @cpt-dod:cpt-cf-oagw-dod-alias-derivation:p1

use std::net::IpAddr;

use crate::domain::alias::{Alias, Hostname};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::scheme::Scheme;
use crate::domain::upstream::Endpoint;

/// Declared standard port for an `http` endpoint.
const STANDARD_HTTP_PORT: u16 = 80;
/// Declared standard port for an `https`, `wss`, `wt`, or `grpc` endpoint.
const STANDARD_TLS_PORT: u16 = 443;
/// The smallest number of labels a derived multi-host suffix may carry.
const MIN_SUFFIX_LABELS: usize = 2;

/// Why an endpoint set admits no derived alias.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeriveError {
    /// The set holds an IP literal, the hostnames share no suffix of at least
    /// two labels, or the common suffix is itself a bare public suffix.
    NonDerivable,
}

impl DeriveError {
    /// The detail a validation error carries for this outcome; it names the
    /// endpoint set and echoes no value.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::NonDerivable => "server.endpoints is not derivable",
        }
    }
}

/// The declared standard port of an endpoint scheme.
#[must_use]
pub const fn standard_port(scheme: Scheme) -> u16 {
    match scheme {
        Scheme::Http => STANDARD_HTTP_PORT,
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => STANDARD_TLS_PORT,
    }
}

/// Resolves the alias an upstream write stores.
///
/// `stored` is `Some` on a replacement, where the alias is immutable.
///
/// # Errors
///
/// Returns a gateway validation error when the endpoint set derives no alias
/// and the caller supplied none, when the supplied alias differs from the
/// derived one, or when the supplied alias is not a valid alias. Returns the
/// `AliasConflict` catalogue row — which answers 409 — when a replacement
/// would change the stored alias.
#[allow(clippy::result_large_err)]
pub fn resolve(
    endpoints: &[Endpoint],
    supplied: Option<&str>,
    stored: Option<&Alias>,
) -> Result<Alias, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-return
    let alias = match derive(endpoints) {
        Ok(derived) => match supplied {
            Some(supplied) => reconcile(supplied, &derived)?,
            None => derived,
        },
        Err(error) => match supplied {
            // A non-derivable set requires, and accepts, an explicit alias.
            Some(supplied) => Alias::parse(supplied)
                .map_err(|_| require_explicit(error))?,
            None => return Err(require_explicit(error)),
        },
    };
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-return

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-put-if
    if let Some(stored) = stored {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-put
        confirm_immutable(&alias, stored)?;
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-put
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-put-if

    Ok(alias)
}

/// Derives the alias an endpoint set resolves to.
///
/// A single hostname derives itself on a standard port and `hostname:port`
/// otherwise. Several hostnames derive the longest common suffix of at least
/// two labels, or that suffix with the port appended when the port is
/// non-standard, and a bare public suffix is never an alias. An IP literal in
/// the set makes the whole set non-derivable.
///
/// # Errors
///
/// Returns [`DeriveError::NonDerivable`] when the set admits no alias; the
/// caller then requires an explicit one.
pub fn derive(endpoints: &[Endpoint]) -> Result<Alias, DeriveError> {
    let mut hostnames = Vec::with_capacity(endpoints.len());
    let mut ip_literal = false;
    for endpoint in endpoints {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-normalize
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-classify
        match endpoint.host.as_str().parse::<IpAddr>() {
            Ok(_) => ip_literal = true,
            Err(_) => match Hostname::parse(endpoint.host.as_str()) {
                Ok(name) => hostnames.push(name),
                Err(_) => return Err(DeriveError::NonDerivable),
            },
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-classify
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-normalize
    }

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-nd-if
    if ip_literal || hostnames.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-nd
        return Err(DeriveError::NonDerivable);
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-nd
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-nd-if

    let Some(first) = endpoints.first() else {
        return Err(DeriveError::NonDerivable);
    };
    let port = endpoint_port(first);
    let standard = port == standard_port(first.scheme);

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-single-if
    if hostnames.len() == 1 {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-single
        return single(&hostnames[0], port, standard);
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-single
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-single-if

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-multi-if
    if hostnames.len() > 1 {
        return multi(&hostnames, port, standard);
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-multi-if

    Err(DeriveError::NonDerivable)
}

/// The effective port of an endpoint, with the declared default applied.
fn endpoint_port(endpoint: &Endpoint) -> u16 {
    endpoint
        .port
        .unwrap_or_else(|| standard_port(endpoint.scheme))
}

/// Derives the alias of a single-hostname endpoint set.
fn single(host: &Hostname, port: u16, standard: bool) -> Result<Alias, DeriveError> {
    if standard {
        Alias::parse(host.as_str()).map_err(|_| DeriveError::NonDerivable)
    } else {
        Alias::parse(&format!("{}:{port}", host.as_str()))
            .map_err(|_| DeriveError::NonDerivable)
    }
}

/// Derives the alias of a multi-hostname endpoint set.
fn multi(hostnames: &[Hostname], port: u16, standard: bool) -> Result<Alias, DeriveError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-suffix
    let Some(suffix) = common_suffix(hostnames) else {
        return Err(DeriveError::NonDerivable);
    };
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-suffix

    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-suffix-if
    if bare_public_suffix(&suffix) {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-suffix-reject
        return Err(DeriveError::NonDerivable);
        // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-suffix-reject
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-suffix-if

    if standard {
        Alias::parse(&suffix).map_err(|_| DeriveError::NonDerivable)
    } else {
        Alias::parse(&format!("{suffix}:{port}")).map_err(|_| DeriveError::NonDerivable)
    }
}

/// The longest common suffix of at least two labels, comparing the reversed
/// label sequences.
fn common_suffix(hostnames: &[Hostname]) -> Option<String> {
    let mut sequences: Vec<Vec<&str>> = hostnames
        .iter()
        .map(|host| host.as_str().split('.').rev().collect())
        .collect();
    let first = sequences.pop()?;
    let first: &[&str] = first.as_slice();
    let rest: &[Vec<&str>] = sequences.as_slice();

    let mut shared: Vec<String> = Vec::new();
    for (position, label) in first.iter().enumerate() {
        if !rest
            .iter()
            .all(|labels| labels.get(position) == Some(label))
        {
            break;
        }
        shared.push((*label).to_owned());
    }

    if shared.len() < MIN_SUFFIX_LABELS {
        return None;
    }

    Some(
        shared
            .iter()
            .rev()
            .fold(String::new(), |accumulated, label| {
                if accumulated.is_empty() {
                    label.clone()
                } else {
                    format!("{accumulated}.{label}")
                }
            }),
    )
}

/// Whether a candidate suffix is itself a bare public suffix, which is never
/// an alias.
fn bare_public_suffix(candidate: &str) -> bool {
    psl::suffix_str(candidate) == Some(candidate)
}

/// Reconciles a caller-supplied alias with the derived one.
///
/// A derivable endpoint set does not leave the alias free: an equal supplied
/// value is accepted as an idempotent no-op, a different one is a validation
/// error.
///
/// # Errors
///
/// Returns a gateway validation error when the supplied value is not a valid
/// alias or differs from the derived one.
#[allow(clippy::result_large_err)]
fn reconcile(supplied: &str, derived: &Alias) -> Result<Alias, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-supplied-if
    let parsed = Alias::parse(supplied).map_err(|_| alias_mismatch())?;
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-supplied-eq
    if parsed == *derived {
        return Ok(parsed);
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-supplied-eq
    // @cpt-begin:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-supplied-ne
    Err(alias_mismatch())
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-supplied-ne
    // @cpt-end:cpt-cf-oagw-algo-alias-derive:p1:inst-alias-derive-supplied-if
}

/// The validation error for a supplied alias the endpoint set does not derive.
fn alias_mismatch() -> DomainError {
    DomainError::gateway(
        ErrorKind::ValidationError,
        "alias does not match the endpoint set",
    )
}

/// The validation error the FEATURE's non-derivable step states, naming the
/// endpoint set and echoing no value.
fn require_explicit(error: DeriveError) -> DomainError {
    DomainError::gateway(ErrorKind::ValidationError, error.detail())
}

/// Confirms the alias is immutable across a replacement.
///
/// # Errors
///
/// Returns the `AliasConflict` catalogue row, which answers 409, when the
/// replacement endpoints derive a different alias; the stored alias is left
/// unchanged.
#[allow(clippy::result_large_err)]
pub fn confirm_immutable(derived: &Alias, stored: &Alias) -> Result<(), DomainError> {
    if derived == stored {
        return Ok(());
    }
    Err(DomainError::gateway(
        ErrorKind::AliasConflict,
        "alias is immutable across updates",
    ))
}
