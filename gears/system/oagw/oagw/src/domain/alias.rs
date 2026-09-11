//! Alias derivation and alias immutability (DESIGN §5.5, PRD §5.5).

use super::error::DomainError;
use super::model::Endpoint;

/// Derive the routing alias for an upstream from its endpoints.
///
/// | endpoints | alias |
/// |---|---|
/// | one, standard port | `host` |
/// | one, non-standard port | `host:port` |
/// | several with a common suffix (≥2 labels), standard port | common suffix |
/// | several with common suffix, non-standard port | `common-suffix:port` |
///
/// Bare public suffixes (`com`, `co.uk`) and IP literals are rejected as non-derivable; so are
/// endpoint pools with differing ports.
pub fn derive_alias(endpoints: &[Endpoint]) -> Result<String, DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::Validation(
            "alias cannot be derived: no endpoints configured".to_string(),
        ));
    }
    let normalized: Vec<Endpoint> = endpoints.iter().map(normalize_endpoint).collect();

    if let [only] = normalized.as_slice() {
        return alias_for(&only.host, only.effective_port());
    }

    let port = normalized[0].effective_port();
    if normalized.iter().any(|ep| ep.effective_port() != port) {
        return Err(DomainError::Validation(
            "alias cannot be derived: endpoints use differing ports".to_string(),
        ));
    }
    let hosts: Vec<&str> = normalized.iter().map(|e| e.host.as_str()).collect();
    let suffix = common_suffix(&hosts).ok_or_else(|| {
        DomainError::Validation(
            "endpoints share no common suffix of at least two labels".to_string(),
        )
    })?;
    reject_bare_public_suffix(&suffix)?;
    alias_for(&suffix, port)
}

fn normalize_endpoint(ep: &Endpoint) -> Endpoint {
    Endpoint {
        scheme: ep.scheme,
        host: normalize_host(&ep.host),
        port: ep.port,
    }
}

fn alias_for(host: &str, port: u16) -> Result<String, DomainError> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err(DomainError::Validation(
            "alias cannot be derived from an IP endpoint; supply an explicit alias".to_string(),
        ));
    }
    reject_bare_public_suffix(host)?;
    if port == 80 || port == 443 {
        Ok(host.to_string())
    } else {
        Ok(format!("{host}:{port}"))
    }
}

#[must_use]
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Longest common label suffix shared by every host, when it has at least two labels.
#[must_use]
pub fn common_suffix(hosts: &[&str]) -> Option<String> {
    let mut labels: Vec<Vec<&str>> = hosts.iter().map(|h| h.split('.').collect()).collect();
    let first = labels.pop()?;
    if first.len() < 2 {
        return None;
    }
    let mut kept = 0;
    for offset in 1..=first.len() {
        let label = first[first.len() - offset];
        let shared = labels.iter().all(|ls| {
            ls.len()
                .checked_sub(offset)
                .and_then(|idx| ls.get(idx))
                .is_some_and(|other| *other == label)
        });
        if shared {
            kept = offset;
        } else {
            break;
        }
    }
    if kept < 2 {
        return None;
    }
    Some(first[first.len() - kept..].join("."))
}

fn reject_bare_public_suffix(host: &str) -> Result<(), DomainError> {
    if psl::suffix_str(host) == Some(host) {
        return Err(DomainError::Validation(format!(
            "'{host}' is a public suffix; an explicit alias is required"
        )));
    }
    Ok(())
}

/// True when `candidate` equals the alias the endpoints derive.
///
/// A caller may repeat the derived alias (idempotent no-op); a different value is only accepted for
/// endpoint sets the derivation cannot handle.
#[must_use]
pub fn alias_matches_derivation(candidate: &str, endpoints: &[Endpoint]) -> bool {
    derive_alias(endpoints)
        .map(|derived| normalize_alias(candidate) == derived)
        .unwrap_or(false)
}

/// ASCII-lowercase an alias and strip its trailing dot.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// True when the two aliases are the same after normalization.
#[must_use]
pub fn aliases_equal(a: &str, b: &str) -> bool {
    normalize_alias(a) == normalize_alias(b)
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod tests;
