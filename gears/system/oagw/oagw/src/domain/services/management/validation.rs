use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, ListQuery, MatchRules, Route};
use crate::domain::repo::RouteRepository;
use crate::domain::ssrf::SsrfGuard;
use uuid::Uuid;

/// Ensure exactly one of `http` or `grpc` is present in the match rules.
///
/// Rejects routes where both fields are `None` (matches nothing) or both
/// are `Some` (ambiguous protocol).
pub(in crate::domain::services) fn validate_match_rules(
    rules: &MatchRules,
) -> Result<(), DomainError> {
    match (&rules.http, &rules.grpc) {
        (None, None) => Err(DomainError::validation(
            "match rules must specify exactly one of 'http' or 'grpc'",
        )),
        (Some(_), Some(_)) => Err(DomainError::validation(
            "match rules must specify exactly one of 'http' or 'grpc', not both",
        )),
        _ => Ok(()),
    }
}

/// Validate the endpoint list for a server configuration.
///
/// Rules:
/// - At least one endpoint is required.
/// - All endpoints must use either IP addresses or hostnames — no mixing.
/// - All endpoints must share the same scheme (upstream-level invariant).
pub(in crate::domain::services) fn validate_endpoints(
    endpoints: &[Endpoint],
) -> Result<(), DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::validation(
            "server must have at least one endpoint",
        ));
    }

    // IPv6 endpoints are not yet supported — reject early with a clear message.
    // SSRF protections for IPv6 (link-local, ULA, IPv4-mapped, etc.) are in
    // place (`ssrf.rs`), but the proxy and endpoint infrastructure has not been
    // tested with IPv6 upstream addresses yet.
    for (i, ep) in endpoints.iter().enumerate() {
        if ep.normalized_host().parse::<std::net::Ipv6Addr>().is_ok() {
            return Err(DomainError::validation(format!(
                "endpoint[{i}] uses IPv6 address '{}'; IPv6 endpoints are not yet supported",
                ep.host
            )));
        }
    }

    // Check all-IP vs all-hostname consistency.
    let ip_count = endpoints.iter().filter(|ep| ep.is_ip()).count();
    if ip_count != 0 && ip_count != endpoints.len() {
        return Err(DomainError::validation(
            "all endpoints must use either IP addresses or hostnames; mixed configurations are not allowed",
        ));
    }

    // Validate hostname format (RFC 1123) for non-IP endpoints.
    if ip_count == 0 {
        for (i, ep) in endpoints.iter().enumerate() {
            validate_hostname(i, &ep.host)?;
        }
    }

    // Enforce identical scheme and port across the pool.
    if endpoints.len() > 1 {
        let first_scheme = &endpoints[0].scheme;
        let first_port = endpoints[0].port;
        for (i, ep) in endpoints.iter().enumerate().skip(1) {
            if ep.scheme != *first_scheme {
                return Err(DomainError::validation(format!(
                    "endpoint[{i}] scheme {:?} differs from endpoint[0] scheme {:?}; all endpoints must share the same scheme",
                    ep.scheme, first_scheme
                )));
            }
            if ep.port != first_port {
                return Err(DomainError::validation(format!(
                    "endpoint[{i}] port {} differs from endpoint[0] port {}; all endpoints must share the same port",
                    ep.port, first_port
                )));
            }
        }
    }

    Ok(())
}

/// Validate endpoints against the SSRF guard.
///
/// Rejects endpoints whose host is a blocked hostname or whose IP address
/// is denied by the built-in or extra deny-lists (unless allow-listed).
/// When the guard is disabled, this is a no-op.
pub(in crate::domain::services) fn validate_endpoints_ssrf(
    guard: &SsrfGuard,
    endpoints: &[Endpoint],
) -> Result<(), DomainError> {
    if !guard.is_enabled() {
        return Ok(());
    }
    for (i, ep) in endpoints.iter().enumerate() {
        let host = ep.normalized_host();
        if let Some(blocked) = guard.is_hostname_blocked(&host) {
            return Err(DomainError::validation(format!(
                "endpoint[{i}] hostname '{}' is blocked by SSRF protection (matches '{blocked}')",
                ep.host,
            )));
        }
        if let Ok(ip) = host.parse::<std::net::IpAddr>()
            && guard.is_ip_blocked(ip)
        {
            return Err(DomainError::validation(format!(
                "endpoint[{i}] IP address '{}' is blocked by SSRF protection: {}",
                ep.host,
                guard.ip_block_reason(ip),
            )));
        }
    }
    Ok(())
}

/// Validate a hostname per RFC 1123: max 253 chars total, each label 1–63 chars,
/// labels contain only ASCII alphanumeric + hyphen, labels don't start/end with
/// hyphen. A trailing dot (FQDN) is tolerated and stripped before validation.
fn validate_hostname(index: usize, host: &str) -> Result<(), DomainError> {
    let h = host.strip_suffix('.').unwrap_or(host);
    if h.is_empty() {
        return Err(DomainError::validation(format!(
            "endpoint[{index}] host is empty"
        )));
    }
    if h.len() > 253 {
        return Err(DomainError::validation(format!(
            "endpoint[{index}] host '{}' exceeds 253 characters",
            host
        )));
    }
    for label in h.split('.') {
        if label.is_empty() {
            return Err(DomainError::validation(format!(
                "endpoint[{index}] host '{host}' contains an empty label"
            )));
        }
        if label.len() > 63 {
            return Err(DomainError::validation(format!(
                "endpoint[{index}] host '{host}' label '{label}' exceeds 63 characters"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(DomainError::validation(format!(
                "endpoint[{index}] host '{host}' label '{label}' contains invalid characters; \
                 only ASCII alphanumeric and '-' are allowed"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainError::validation(format!(
                "endpoint[{index}] host '{host}' label '{label}' must not start or end with '-'"
            )));
        }
    }
    Ok(())
}

/// Check that no existing **enabled** route under the same upstream shares
/// `(path_prefix, priority, method)` with the candidate route.
///
/// `exclude_id` is `Some(route.id)` on update to skip the route being
/// modified (it will be compared against its new state, not itself).
///
/// Returns `DomainError::Conflict` on violation (maps to 409).
pub(in crate::domain::services) async fn check_route_overlap(
    routes: &dyn RouteRepository,
    candidate: &Route,
    exclude_id: Option<Uuid>,
) -> Result<(), DomainError> {
    // Disabled routes cannot cause match-time ambiguity.
    if !candidate.enabled {
        return Ok(());
    }

    let candidate_http = match &candidate.match_rules.http {
        Some(h) => h,
        None => return Ok(()), // No HTTP match rules → no overlap to check.
    };

    // Fetch all routes for this (tenant, upstream).
    let all = routes
        .list(
            candidate.tenant_id,
            Some(candidate.upstream_id),
            &ListQuery {
                top: u32::MAX,
                skip: 0,
            },
        )
        .await
        .map_err(DomainError::from)?;

    for existing in &all {
        // Skip self on update.
        if Some(existing.id) == exclude_id {
            continue;
        }
        // Only enabled routes can conflict.
        if !existing.enabled {
            continue;
        }
        // Must have HTTP match rules.
        let Some(existing_http) = &existing.match_rules.http else {
            continue;
        };
        // Must share path and priority.
        if existing_http.path != candidate_http.path || existing.priority != candidate.priority {
            continue;
        }
        // Check for any overlapping method.
        for m in &candidate_http.methods {
            if existing_http.methods.contains(m) {
                return Err(DomainError::conflict(
                    "route",
                    format!("{}:{}:{:?}", candidate.upstream_id, candidate_http.path, m),
                    format!(
                        "route overlap: an enabled route already exists on upstream '{}' \
                             with path '{}', priority {}, method {:?}",
                        candidate.upstream_id, candidate_http.path, candidate.priority, m
                    ),
                ));
            }
        }
    }

    Ok(())
}
