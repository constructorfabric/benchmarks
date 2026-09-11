//! Alias derivation and alias immutability
//! (`cpt-cf-oagw-algo-upstream-management-alias-derivation` and
//! `cpt-cf-oagw-algo-upstream-management-alias-immutability`).
//!
//! The derivation is listed in this entry's dependency chain and delivered
//! here so entry 2.2 (upstream management) can consume it without re-deriving
//! the public-suffix rules. It is a pure function over the endpoint pool.

use psl;
use std::net::IpAddr;

use crate::domain::dto::{Endpoint, EndpointScheme, ServerConfig};
use crate::domain::error::DomainError;
use crate::domain::validation::{alias_is_valid, host_is_ip};

/// Why an endpoint pool admits no derived alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivationError {
    /// No endpoint carries a hostname (every host is an IP address).
    NoHostnameEndpoint,
    /// The only common suffix is a bare public suffix (e.g. a two-label
    /// country suffix such as `co.uk`).
    BarePublicSuffix,
    /// The hostnames share no registrable common suffix.
    NoCommonSuffix,
    /// The endpoint pool is empty.
    NoEndpoints,
}

impl AliasDerivationError {
    /// The validation message for the rejection; it names the reason and
    /// never echoes an endpoint host.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoEndpoints => "at least one endpoint is required",
            Self::NoHostnameEndpoint => "an IP-only endpoint pool admits no derived alias",
            Self::BarePublicSuffix => {
                "the only common suffix is a bare public suffix and admits no alias"
            }
            Self::NoCommonSuffix => "the endpoint hostnames share no registrable common suffix",
        }
    }
}

/// One endpoint of the pool as the alias algorithm consumes it: normalized
/// ASCII-lowercase host with the trailing FQDN dot stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedEndpoint {
    host: String,
    port: u16,
    scheme: EndpointScheme,
    is_ip: bool,
}

fn normalize(endpoint: &Endpoint) -> NormalizedEndpoint {
    let host = endpoint
        .host
        .trim()
        .strip_suffix('.')
        .unwrap_or_else(|| endpoint.host.trim())
        .to_ascii_lowercase();
    NormalizedEndpoint {
        is_ip: host.parse::<IpAddr>().is_ok() || host_is_ip(&host),
        host,
        port: endpoint.port,
        scheme: endpoint.scheme,
    }
}

/// The standard port per scheme (derivation step 2): `80` for `http`, `443`
/// for `https`, `wss`, `wt` and `grpc`.
#[must_use]
pub const fn standard_port(scheme: EndpointScheme) -> u16 {
    scheme.standard_port()
}

/// The registrable domain of `host` on the public suffix list, or `None` when
/// the host is an IP address, a bare public suffix, or not registrable.
#[must_use]
pub fn registrable_domain(host: &str) -> Option<String> {
    let bytes = host.as_bytes();
    let domain = psl::domain(bytes)?;
    std::str::from_utf8(domain.as_bytes()).ok().map(str::to_owned)
}

/// Normalize a user-supplied alias: surrounding whitespace trimmed, a single
/// trailing FQDN dot stripped and the remainder ASCII lowercased
/// (`inst-um-cr-11`). The caller confirms the result against the alias pattern
/// with [`crate::domain::validation::alias_is_valid`].
#[must_use]
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-11
// `inst-um-cr-11`: ASCII lowercase with the trailing dot stripped.
pub fn normalize_alias(alias: &str) -> String {
    let trimmed = alias.trim();
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    trimmed.to_ascii_lowercase()
}
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-11

/// Derive the alias of an endpoint pool.
///
/// Steps (algorithm order):
/// 1. normalize every endpoint host to ASCII lowercase, stripping a single
///    trailing dot tolerated as FQDN notation;
/// 2. standard port per scheme: `80` for `http`, `443` for the rest;
/// 3. exactly one hostname endpoint -> the hostname, or `hostname:port` when
///    the port is non-standard;
/// 4. two or more hostname endpoints -> the registrable common suffix, only
///    when it carries a registrable domain on the public suffix list; the
///    alias is the suffix, or `suffix:port` when the pool's port is
///    non-standard;
/// 5. non-derivable for a bare public suffix, no common registrable suffix,
///    or an IP endpoint host;
/// 6. the derived alias is normalized (lowercase, trailing dots stripped) and
///    confirmed against the alias pattern.
///
/// # Errors
///
/// Returns [`DomainError::ValidationError`] naming `alias` when the pool
/// admits no derived alias.
pub fn derive_alias(server: &ServerConfig) -> Result<String, DomainError> {
    match try_derive_alias(server) {
        Ok(Some(alias)) => {
            if alias_is_valid(&alias) {
                Ok(alias)
            } else {
                Err(DomainError::field_rejection(
                    "alias",
                    "the derived alias does not match the alias pattern",
                ))
            }
        }
        Ok(None) => Err(DomainError::field_rejection(
            "alias",
            "the endpoint pool admits no derived alias and requires an explicit alias",
        )),
        Err(reason) => Err(DomainError::field_rejection("alias", reason.reason())),
    }
}

/// The derivation without the error mapping: `Ok(None)` means "non-derivable,
/// an explicit alias is required".
///
/// # Errors
///
/// Returns an [`AliasDerivationError`] for an unusable pool.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-1
// `inst-um-ad-1` .. `-11`: the derivation table — lowercase hosts with the
// trailing dot stripped, the standard port per scheme, the single-hostname and
// registrable-common-suffix verdicts, and the non-derivable cases that make an
// explicit alias mandatory.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-10
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-11
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-2
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-3
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-4
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-5
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-6
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-7
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-8
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-9
pub fn try_derive_alias(server: &ServerConfig) -> Result<Option<String>, AliasDerivationError> {
    if server.endpoints.is_empty() {
        return Err(AliasDerivationError::NoEndpoints);
    }
    let endpoints: Vec<NormalizedEndpoint> = server.endpoints.iter().map(normalize).collect();

    // Step 5: any IP endpoint host makes the pool non-derivable.
    if endpoints.iter().any(|e| e.is_ip) {
        return Err(AliasDerivationError::NoHostnameEndpoint);
    }

    // The pool is uniform in port (validated at write time), so one port
    // describes the pool.
    let pool_port = endpoints[0].port;
    let scheme = endpoints[0].scheme;
    let standard = pool_port == standard_port(scheme);

    if endpoints.len() == 1 {
        // Step 3.
        let only = &endpoints[0];
        return Ok(Some(if standard {
            only.host.clone()
        } else {
            format!("{}:{pool_port}", only.host)
        }));
    }

    // Step 4: the registrable common suffix of every hostname.
    let hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
    let common = common_suffix_labels(&hosts);
    if common.is_empty() {
        return Err(AliasDerivationError::NoCommonSuffix);
    }
    let suffix = common.join(".");
    // A bare public suffix (`co.uk`) is not a registrable domain, and neither
    // is a suffix the list does not know at all.
    if registrable_domain(&suffix).is_none() {
        return Err(AliasDerivationError::BarePublicSuffix);
    }
    Ok(Some(if standard {
        suffix
    } else {
        format!("{suffix}:{pool_port}")
    }))
}
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-9
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-8
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-7
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-6
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-5
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-4
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-2
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-11
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-10
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-derivation:p1:inst-um-ad-1

/// The longest common label suffix of every host, longest-last (the labels of
/// `us.vendor.com` and `eu.vendor.com` are `["vendor", "com"]`).
fn common_suffix_labels(hosts: &[String]) -> Vec<String> {
    let mut common: Vec<String> =
        hosts[0].split('.').rev().map(str::to_owned).collect();
    for host in &hosts[1..] {
        let labels: Vec<&str> = host.split('.').rev().collect();
        let keep = common.len().min(labels.len());
        let mut next = Vec::with_capacity(keep);
        for (index, label) in common.iter().take(keep).enumerate() {
            if label == labels[index] {
                next.push(label.clone());
            } else {
                break;
            }
        }
        common = next;
        if common.is_empty() {
            break;
        }
    }
    common.reverse();
    common
}

/// Alias-immutability classification of an endpoint pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasClass {
    /// The pool admits a derived alias.
    Derivable,
    /// The pool admits none (IP-only, bare public suffix, or no common
    /// suffix).
    NonDerivable,
}

/// Classify a pool (immutability algorithm step 1).
#[must_use]
pub fn alias_class(server: &ServerConfig) -> AliasClass {
    match try_derive_alias(server) {
        Ok(Some(_)) => AliasClass::Derivable,
        Ok(None) | Err(_) => AliasClass::NonDerivable,
    }
}

/// The alias to store for a replacement upstream, per the alias-immutability
/// transition matrix
/// (`cpt-cf-oagw-algo-upstream-management-alias-immutability`).
///
/// `stored` is the current record (its pool classifies the stored side and its
/// alias is the value to retain); `replacement` carries the replacement pool
/// and any supplied alias, which is treated as *not supplied* when empty.
///
/// * unchanged pool and no supplied alias -> the stored alias is retained;
/// * unchanged pool and an equal supplied alias -> idempotent no-op;
/// * unchanged pool and a differing supplied alias -> rejected: an alias
///   override is not permitted;
/// * derivable -> derivable: accepted only when the recomputed alias equals
///   the stored alias, otherwise "delete and re-create";
/// * derivable -> non-derivable: rejected always, even when the supplied
///   alias equals the stored alias;
/// * non-derivable -> non-derivable: the stored alias stands and a differing
///   supplied alias is rejected;
/// * non-derivable -> derivable: accepted only when the derived alias equals
///   the stored alias.
///
/// # Errors
///
/// Returns a validation error naming `alias` when the supplied alias is not
/// acceptable.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-1
// `inst-um-ai-1` .. `-16`: the alias-immutability matrix over the stored and
// the replacement pool, with the delete-and-re-create remediation on every
// rejection.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-10
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-11
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-12
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-13
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-14
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-15
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-16
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-2
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-3
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-4
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-5
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-6
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-7
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-8
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-9
pub fn validate_alias_replacement(
    stored: &crate::domain::dto::Upstream,
    replacement: &crate::domain::dto::Upstream,
) -> Result<String, DomainError> {
    let supplied = replacement.alias.as_str();
    let stored_alias = stored.alias.as_str();
    let stored_class = alias_class(&stored.server);
    let replacement_class = alias_class(&replacement.server);

    // An unchanged pool is the idempotency case, decided on the supplied
    // alias alone.
    if stored.server == replacement.server {
        return if supplied.is_empty() || supplied == stored_alias {
            Ok(stored_alias.to_owned())
        } else {
            Err(DomainError::field_rejection(
                "alias",
                "the alias is immutable and an alias override is not permitted",
            ))
        };
    }

    match (stored_class, replacement_class) {
        (AliasClass::Derivable, AliasClass::Derivable)
        | (AliasClass::NonDerivable, AliasClass::Derivable) => {
            match try_derive_alias(&replacement.server) {
                Ok(Some(derived)) if derived == stored_alias => Ok(stored_alias.to_owned()),
                Ok(Some(_)) | Err(_) => Err(DomainError::field_rejection(
                    "alias",
                    "the alias is immutable: delete and re-create the upstream to change it",
                )),
                Ok(None) => Err(DomainError::field_rejection(
                    "alias",
                    "the endpoint pool admits no derived alias and requires an explicit alias",
                )),
            }
        }
        // Derivable -> non-derivable: rejected always.
        (AliasClass::Derivable, AliasClass::NonDerivable) => Err(DomainError::field_rejection(
            "alias",
            "the alias is immutable: delete and re-create the upstream to change it",
        )),
        // Non-derivable -> non-derivable: the stored alias stands.
        (AliasClass::NonDerivable, AliasClass::NonDerivable) => {
            if supplied.is_empty() || supplied == stored_alias {
                Ok(stored_alias.to_owned())
            } else {
                Err(DomainError::field_rejection(
                    "alias",
                    "the alias is immutable and an alias override is not permitted",
                ))
            }
        }
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-9
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-8
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-7
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-6
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-5
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-4
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-2
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-16
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-15
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-14
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-13
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-12
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-11
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-10
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-alias-immutability:p1:inst-um-ai-1

#[cfg(test)]
#[path = "alias_tests.rs"]
mod tests;
