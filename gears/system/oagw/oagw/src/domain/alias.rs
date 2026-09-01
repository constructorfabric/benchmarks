//! Alias derivation and enforcement.
//!
//! Aliases are the routing key of `/oagw/v1/proxy/{alias}/...`, so they are not
//! arbitrary labels: they are either auto-derived from the endpoint pool or,
//! where derivation is impossible, supplied explicitly by the operator. The
//! derivation table lives in `DESIGN` §3.2 (Alias Enforcement Rules) and the
//! update matrix in `DESIGN` §3.2 (Alias Update Behavior).

use crate::domain::error::DomainError;

/// Minimum label count for a common suffix to be usable as a shared alias.
const MIN_SUFFIX_LABELS: usize = 2;

/// Lowercase an alias and strip the trailing root dot.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Validate an alias against `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the alias is empty, too long, or
/// contains characters outside the accepted set.
pub fn validate_alias_shape(alias: &str) -> Result<(), DomainError> {
    if alias.is_empty() {
        return Err(DomainError::validation("alias must not be empty"));
    }
    if alias.len() > 253 {
        return Err(DomainError::validation(
            "alias must be at most 253 characters",
        ));
    }
    let first = alias.chars().next().unwrap_or(' ');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(DomainError::validation(format!(
            "alias must start with a lowercase letter or digit: {alias}"
        )));
    }
    let last = alias.chars().next_back().unwrap_or(' ');
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return Err(DomainError::validation(format!(
            "alias must end with a lowercase letter or digit: {alias}"
        )));
    }
    for c in alias.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == ':' || c == '-') {
            return Err(DomainError::validation(format!(
                "alias must contain only lowercase letters, digits, dots, colons and hyphens: \
                 {alias}"
            )));
        }
    }
    Ok(())
}

/// Derive the alias of an endpoint pool, or `None` when the pool does not
/// determine one.
///
/// * A single hostname endpoint derives its hostname (plus `:port` when
///   non-standard). IP endpoints never derive.
/// * A pool of hostnames derives its registrable common suffix (plus `:port`
///   when non-standard). A bare public suffix (`co.uk`) never derives.
#[must_use]
pub fn compute_derived_alias(endpoints: &[crate::domain::model::Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }

    let port = endpoints[0].port;
    let scheme = endpoints[0].scheme;
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_alias(&e.host)).collect();
    if hosts.iter().any(String::is_empty) {
        return None;
    }

    let port_suffix = if port == scheme.standard_port() {
        String::new()
    } else {
        format!(":{port}")
    };

    if hosts.len() == 1 {
        let host = &hosts[0];
        return if is_ip(host) {
            None
        } else {
            Some(format!("{host}{port_suffix}"))
        };
    }

    if endpoints.iter().any(|e| e.port != port) {
        return None;
    }
    if hosts.iter().any(|h| is_ip(h)) {
        return None;
    }

    let suffix = common_suffix(&hosts)?;
    Some(format!("{suffix}{port_suffix}"))
}

/// Longest common *registrable* label suffix of a set of lowercase hostnames.
fn common_suffix(hosts: &[String]) -> Option<String> {
    let first: Vec<&str> = hosts[0].split('.').collect();
    let mut shared: Vec<&str> = Vec::new();
    'labels: for depth in 1..=first.len() {
        let candidate = first[first.len() - depth];
        for host in hosts.iter().skip(1) {
            let labels: Vec<&str> = host.split('.').collect();
            if labels.len() < depth || labels[labels.len() - depth] != candidate {
                break 'labels;
            }
        }
        shared.push(candidate);
    }

    if shared.len() < MIN_SUFFIX_LABELS {
        return None;
    }
    let suffix = shared.iter().rev().copied().collect::<Vec<_>>().join(".");
    // A shared suffix must be registrable: it may not be a bare public suffix
    // such as `co.uk`, and it needs at least two labels.
    if suffix.split('.').count() < MIN_SUFFIX_LABELS {
        return None;
    }
    if psl::domain_str(&suffix) != Some(suffix.as_str()) {
        return None;
    }
    Some(suffix)
}

/// `true` when `host` parses as an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Decide the alias of a **new** upstream from the requested alias and the
/// endpoint pool.
///
/// # Errors
/// * [`DomainError::Validation`] when an explicit alias does not match the
///   derived alias of a derivable pool.
/// * [`DomainError::Validation`] when the pool is non-derivable and no explicit
///   alias was supplied.
pub fn enforce_alias_create(
    requested: Option<&str>,
    endpoints: &[crate::domain::model::Endpoint],
) -> Result<String, DomainError> {
    let derived = compute_derived_alias(endpoints);
    match (requested, derived) {
        (Some(raw), Some(derived)) => {
            let normalized = normalize_alias(raw);
            validate_alias_shape(&normalized)?;
            if normalized != derived {
                return Err(DomainError::validation(format!(
                    "alias is derived from the endpoint host and must be '{derived}', \
                     not '{normalized}'; hostname-based upstreams cannot be renamed"
                )));
            }
            Ok(derived)
        }
        (Some(raw), None) => {
            let normalized = normalize_alias(raw);
            validate_alias_shape(&normalized)?;
            Ok(normalized)
        }

        (None, Some(derived)) => Ok(derived),
        (None, None) => Err(DomainError::validation(
            "alias is required: IP-based or non-derivable endpoints do not determine an alias",
        )),
    }
}

/// Enforce the alias rules of a **replacement**.
///
/// The alias is immutable once set — it is the routing key of the proxy API —
/// so the only accepted transition is "the recomputed alias equals the existing
/// one". An explicit alias that matches is a tolerated no-op.
///
/// # Errors
/// * [`DomainError::Validation`] when the new endpoints would change the
///   derived alias.
/// * [`DomainError::Validation`] when the pool loses derivability
///   (hostname → IP): that transition is always rejected, even when an explicit
///   alias is supplied.
/// * [`DomainError::Validation`] when an explicit alias differs from the
///   existing alias.
pub fn enforce_alias_update(
    existing: &str,
    requested: Option<&str>,
    previous: &[crate::domain::model::Endpoint],
    next: &[crate::domain::model::Endpoint],
) -> Result<String, DomainError> {
    // The explicit alias is checked first and unconditionally: the routing key
    // never changes, so an operator-supplied alias that differs from the one
    // already serving traffic is a rename attempt even when the endpoint pool
    // itself still derives the original.
    if let Some(requested) = requested {
        let normalized = normalize_alias(requested);
        validate_alias_shape(&normalized)?;
        if normalized != existing {
            return Err(DomainError::validation(format!(
                "alias is immutable: expected '{existing}', got '{normalized}'"
            )));
        }
    }
    if let Some(derived) = compute_derived_alias(next) {
        if derived != existing {
            return Err(DomainError::validation(format!(
                "alias is immutable: these endpoints would derive '{derived}' but the \
                 upstream is routed as '{existing}'; delete and re-create the upstream"
            )));
        }
    } else if compute_derived_alias(previous).is_some() {
        // A derivable pool cannot become non-derivable: that transition is
        // refused outright rather than silently keeping the old routing key.
        return Err(DomainError::validation(
            "alias is immutable: a derivable endpoint pool cannot become \
             non-derivable; delete and re-create the upstream",
        ));
    }
    Ok(existing.to_owned())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "alias_tests.rs"]
mod tests;
