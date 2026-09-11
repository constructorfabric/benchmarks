//! Alias derivation, validation and the update-transition matrix (DESIGN §3.2
//! "Alias Resolution", PRD §5.5 "Alias Resolution and Shadowing").
//!
//! Everything here is a pure function of its inputs so it can be exercised by
//! table-driven tests without a store or an HTTP stack.

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme};

/// Why an alias could not be derived from an endpoint set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// A derived alias is available.
    Derived(String),
    /// Derivation failed; an explicit alias is required.
    NotDerivable(&'static str),
}

/// Maximum length of an RFC 1123 hostname.
const MAX_HOST_LEN: usize = 253;
/// Maximum length of an RFC 1123 hostname label.
const MAX_LABEL_LEN: usize = 63;

/// `true` when `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok() || host.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Validates an endpoint host per RFC 1123 (or accepts an IP literal).
///
/// A trailing dot (FQDN notation) is tolerated and stripped.
///
/// # Errors
/// Returns [`DomainError::Validation`] describing the first violated rule.
pub fn validate_host(host: &str) -> Result<(), DomainError> {
    if host.is_empty() {
        return Err(DomainError::Validation(
            "endpoint host must not be empty".to_owned(),
        ));
    }
    if is_ip_address(host) {
        return Ok(());
    }
    if host.len() > MAX_HOST_LEN {
        return Err(DomainError::Validation(format!(
            "endpoint host `{host}` exceeds 253 characters"
        )));
    }
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() {
        return Err(DomainError::Validation(format!(
            "endpoint host `{host}` is not a valid hostname"
        )));
    }
    for label in trimmed.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(DomainError::Validation(format!(
                "endpoint host `{host}` has an invalid label"
            )));
        }
        let valid = label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !valid {
            return Err(DomainError::Validation(format!(
                "endpoint host `{host}` is not a valid RFC 1123 hostname"
            )));
        }
    }
    Ok(())
}

/// Normalizes a host: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let lower = host.trim().to_ascii_lowercase();
    lower.strip_suffix('.').unwrap_or(&lower).to_owned()
}

/// Normalizes an alias: ASCII lowercase, whitespace trimmed, trailing dots
/// stripped. Resolution is therefore case-insensitive.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    let lower = alias.trim().to_ascii_lowercase();
    let stripped = lower.strip_suffix('.').unwrap_or(&lower);
    stripped.to_owned()
}

/// Whether `port` is the standard port for `scheme` and thus omitted from a
/// derived alias.
#[must_use]
pub fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    port == scheme.standard_port()
}

/// The alias suffix contributed by a non-standard port (`":8443"` or `""`).
#[must_use]
fn port_suffix(endpoint: &Endpoint) -> String {
    let port = endpoint.effective_port();
    if is_standard_port(endpoint.scheme, port) {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// Computes the derived alias for an endpoint set.
///
/// * one hostname endpoint → `hostname[:port]`
/// * several hostname endpoints sharing a registrable common suffix → `suffix[:port]`
/// * IP-only sets, bare-public-suffix sets and heterogeneous sets are not derivable
///
/// # Errors
/// Returns [`DomainError::Validation`] when the endpoints themselves are invalid.
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Result<AliasDerivation, DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::Validation(
            "upstream requires at least one server endpoint".to_owned(),
        ));
    }

    let mut hosts = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        validate_host(&ep.host)?;
        hosts.push(normalize_host(&ep.host));
    }

    // Every pool endpoint must agree on the port, so the port suffix is unambiguous.
    let ports: Vec<u16> = endpoints.iter().map(Endpoint::effective_port).collect();
    let schemes: Vec<EndpointScheme> = endpoints.iter().map(|e| e.scheme).collect();
    if !schemes.iter().all(|s| *s == schemes[0]) || !ports.iter().all(|p| *p == ports[0]) {
        return Err(DomainError::Validation(
            "all endpoints of an upstream must share the same scheme and port".to_owned(),
        ));
    }

    let ip_count = hosts.iter().filter(|h| is_ip_address(h)).count();
    if ip_count > 0 {
        if ip_count != hosts.len() {
            return Ok(AliasDerivation::NotDerivable(
                "endpoint pool mixes hostnames and IP addresses",
            ));
        }
        return Ok(AliasDerivation::NotDerivable(
            "IP-address endpoints require an explicit alias",
        ));
    }

    let unique: Vec<&str> = {
        let mut v: Vec<&str> = hosts.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    if unique.len() == 1 {
        return Ok(AliasDerivation::Derived(format!(
            "{}{}",
            unique[0],
            port_suffix(&endpoints[0])
        )));
    }

    let suffix = common_registrable_suffix(&unique);
    match suffix {
        Some(s) => Ok(AliasDerivation::Derived(format!(
            "{s}{}",
            port_suffix(&endpoints[0])
        ))),
        None => Ok(AliasDerivation::NotDerivable(
            "hostname pool has no common registrable suffix",
        )),
    }
}

/// The longest common *registrable* suffix of a hostname set, when one exists.
///
/// Registrable is checked against the Public Suffix List, so `foo.co.uk` +
/// `bar.co.uk` yields `None` (`co.uk` is a bare public suffix).
#[must_use]
pub fn common_registrable_suffix(hosts: &[&str]) -> Option<String> {
    let mut roots: Vec<String> = Vec::with_capacity(hosts.len());
    for host in hosts {
        let candidate = normalize_host(host);
        // `domain_str` yields the registrable domain and yields `None` for a bare
        // public suffix, so a pool over `foo.co.uk`/`bar.co.uk` derives nothing.
        let root = psl::domain_str(&candidate)?;
        if root.split('.').count() < 2 {
            return None;
        }
        roots.push(root.to_owned());
    }
    let first = roots[0].clone();
    if roots.iter().all(|r| *r == first) {
        Some(first)
    } else {
        None
    }
}

/// Resolves the alias for a **create** operation.
///
/// * hostname-derived endpoints → the derived alias, with a user-supplied value
///   tolerated only when it matches exactly (idempotent no-op)
/// * non-derivable endpoints → an explicit alias is mandatory
///
/// # Errors
/// Returns [`DomainError::Validation`] on any mismatch or missing alias.
pub fn enforce_alias_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    match compute_derived_alias(endpoints)? {
        AliasDerivation::Derived(derived) => {
            let normalized = provided.map(normalize_alias);
            if let Some(p) = normalized
                && p != derived
            {
                return Err(DomainError::Validation(format!(
                    "alias `{p}` does not match the endpoint-derived alias `{derived}`; \
                         hostname-based endpoints always auto-derive their alias"
                )));
            }
            crate::domain::model::validate_alias_shape(&derived)?;
            Ok(derived)
        }
        AliasDerivation::NotDerivable(reason) => {
            let Some(provided) = provided.map(normalize_alias).filter(|p| !p.is_empty()) else {
                return Err(DomainError::Validation(format!(
                    "an explicit alias is required: {reason}"
                )));
            };
            crate::domain::model::validate_alias_shape(&provided)?;
            Ok(provided)
        }
    }
}

/// Resolves the alias for a **replace** operation.
///
/// The alias is the routing key in `/oagw/v1/proxy/{alias}`, so it is immutable:
/// an endpoint change that would alter the derived alias is rejected and the
/// operator must delete and re-create the upstream.
///
/// # Errors
/// Returns [`DomainError::Validation`] per the transition matrix.
pub fn enforce_alias_update(
    existing_alias: &str,
    existing_derivable: bool,
    new_endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    match compute_derived_alias(new_endpoints)? {
        AliasDerivation::Derived(derived) => {
            if normalize_alias(existing_alias) != derived {
                return Err(DomainError::Validation(format!(
                    "alias is immutable: these endpoints derive `{derived}` but the upstream is \
                     registered as `{existing_alias}`; delete and re-create the upstream"
                )));
            }
            if let Some(p) = provided.map(normalize_alias).filter(|p| !p.is_empty())
                && p != derived
            {
                return Err(DomainError::Validation(format!(
                    "alias is immutable and `{p}` does not match the derived alias `{derived}`"
                )));
            }
            Ok(normalize_alias(existing_alias))
        }
        AliasDerivation::NotDerivable(reason) => {
            if existing_derivable {
                // Derivable → non-derivable is always rejected, even when the
                // operator supplies the existing alias verbatim.
                return Err(DomainError::Validation(
                    "alias is immutable: replacing hostname endpoints with IP endpoints would \
                     change the alias semantics; delete and re-create the upstream"
                        .to_owned(),
                ));
            }
            match provided.map(normalize_alias).filter(|p| !p.is_empty()) {
                None => Ok(normalize_alias(existing_alias)),
                Some(p) if p == normalize_alias(existing_alias) => {
                    Ok(normalize_alias(existing_alias))
                }
                Some(_) => Err(DomainError::Validation(format!(
                    "alias is immutable: {reason}, and a differing alias cannot be supplied on \
                     replace"
                ))),
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod alias_tests;
