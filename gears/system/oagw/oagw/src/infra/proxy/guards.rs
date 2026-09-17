//! Core request guards (DESIGN "Guard Rules").
//!
//! These are *core data-plane logic*, not `GuardPlugin` implementations — only
//! `required_headers` is a resolvable guard plugin (ADR 0009). The guards here
//! run before the rate limiter, so a request that can never be proxied never
//! consumes an allowance.
//!
//! | Guard | Configured by | Failure |
//! |---|---|---|
//! | method | `match.http.methods` | `405` + `Allow` |
//! | query allow-list | `match.http.query_allowlist` | `400` |
//! | path suffix | `match.http.path_suffix_mode` | `400` |
//! | body framing | `Content-Length` / `Transfer-Encoding` | `400` |
//! | body size | `body_limit_bytes` | `413` |
//! | outbound dial | `ssrf_policy` | `403` |

use crate::domain::error::DomainError;
use crate::infra::proxy::resolve::MatchedRoute;

/// Body limits a proxied request must respect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPolicy {
    /// Largest buffered body, in bytes.
    pub limit_bytes: u64,
    /// Whether `Transfer-Encoding` other than `chunked` is accepted.
    pub allow_identity_transfer_encoding: bool,
}

impl BodyPolicy {
    /// A policy with the given byte limit.
    #[must_use]
    pub const fn new(limit_bytes: u64) -> Self {
        Self {
            limit_bytes,
            allow_identity_transfer_encoding: false,
        }
    }
}

/// Evaluate the request guards that depend on the *matched route*.
///
/// The method and path-suffix guards are evaluated during route matching
/// ([`crate::infra::proxy::resolve::match_route`]); this function covers the
/// remaining, transport-level guards.
///
/// # Errors
/// * [`DomainError::PayloadTooLarge`] — the declared body exceeds the limit.
/// * [`DomainError::Validation`] — the framing is inconsistent.
pub fn check_request_guards(
    matched: &MatchedRoute,
    headers: &http::HeaderMap,
    method: &str,
    policy: BodyPolicy,
) -> Result<(), DomainError> {
    check_body_guards(headers, method, policy)?;
    check_query_guard(matched)?;
    Ok(())
}

/// Enforce the body-framing and body-size rules.
///
/// # Errors
/// [`DomainError::PayloadTooLarge`] or [`DomainError::Validation`].
pub fn check_body_guards(
    headers: &http::HeaderMap,
    method: &str,
    policy: BodyPolicy,
) -> Result<(), DomainError> {
    let _ = method;
    if let Some(length) = content_length(headers)?
        && length > policy.limit_bytes
    {
        return Err(DomainError::PayloadTooLarge {
            limit_bytes: policy.limit_bytes,
        });
    }

    if !policy.allow_identity_transfer_encoding {
        for encoding in headers.get_all(http::header::TRANSFER_ENCODING) {
            let Some(raw) = encoding.to_str().ok() else {
                return Err(DomainError::invalid(
                    "transfer-encoding header is not valid ASCII",
                ));
            };
            for token in raw.split(',').map(str::trim).map(str::to_ascii_lowercase) {
                if token.is_empty() {
                    continue;
                }
                if token != "chunked" {
                    return Err(DomainError::invalid(format!(
                        "transfer-encoding `{token}` is not supported; only `chunked` is accepted"
                    )));
                }
            }
        }
    }

    Ok(())
}

/// The declared `Content-Length`, when present *and* a valid number.
///
/// A `Content-Length` that is present but not a valid integer is a framing
/// error, not a policy one; an absent header returns `Ok(None)`.
fn content_length(headers: &http::HeaderMap) -> Result<Option<u64>, DomainError> {
    let Some(value) = headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| DomainError::invalid("content-length header is not valid ASCII"))?
        .trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u64>()
        .map(Some)
        .map_err(|_| DomainError::invalid("content-length header is not a valid integer"))
}

/// True when the request announces a chunked body.
#[must_use]
pub fn is_chunked(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(http::header::TRANSFER_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|raw| {
            raw.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        })
}

/// Re-check the query allow-list of the matched route.
///
/// [`crate::infra::proxy::resolve::match_route`] already rejects unknown query
/// parameters; this hook exists so the guard stage is self-contained and a
/// future caller can run it against a rewritten query.
///
/// # Errors
/// [`DomainError::Validation`] when a parameter is not in the allow-list.
pub fn check_query_guard(matched: &MatchedRoute) -> Result<(), DomainError> {
    let query = matched.upstream_query.as_str();
    if query.is_empty() {
        return Ok(());
    }
    let crate::domain::model::RouteMatch::Http(http) = &matched.route.match_config else {
        return Ok(());
    };
    for name in form_urlencoded::parse(query.as_bytes()).map(|(n, _)| n.into_owned()) {
        if !http
            .query_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&name))
        {
            return Err(DomainError::validation(
                "query",
                format!("query parameter `{name}` is not allowed by this route"),
            ));
        }
    }
    Ok(())
}

/// Outbound dial policy (DESIGN "SSRF guard").
///
/// An *empty* allow-list means "everything not denied": the gateway routes to
/// whatever the tenant configured, and the deny list is the only hard line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrfPolicy {
    /// Whether screening is active.
    pub enabled: bool,
    /// Hosts / CIDRs always allowed (checked before the deny list).
    pub allowed_hosts: Vec<String>,
    /// Hosts / CIDRs always refused.
    pub denied_hosts: Vec<String>,
}

impl SsrfPolicy {
    /// Build the policy from the gear configuration.
    #[must_use]
    pub fn from_config(policy: &crate::config::SsrfPolicyConfig) -> Self {
        Self {
            enabled: policy.enabled,
            allowed_hosts: policy.allowed_hosts.clone(),
            denied_hosts: policy.denied_hosts.clone(),
        }
    }

    /// An enabled policy with no lists (allows everything).
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            enabled: true,
            allowed_hosts: Vec::new(),
            denied_hosts: Vec::new(),
        }
    }

    /// True when `host` may be dialled.
    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if self
            .denied_hosts
            .iter()
            .any(|entry| entry_matches(entry, &host))
        {
            return false;
        }
        if self.allowed_hosts.is_empty() {
            // Empty allow-list: everything not denied is allowed.
            return true;
        }
        self.allowed_hosts
            .iter()
            .any(|entry| entry_matches(entry, &host))
    }

    /// The domain error for a refused host.
    #[must_use]
    pub fn error(&self, host: &str) -> DomainError {
        DomainError::invalid(format!(
            "upstream host `{host}` is not permitted by the outbound SSRF policy"
        ))
    }
}

/// True when a policy entry matches `host` (exact, wildcard `*.suffix`, or CIDR).
fn entry_matches(entry: &str, host: &str) -> bool {
    let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
    if entry.is_empty() {
        return false;
    }
    if let Some((network, prefix)) = parse_cidr(&entry) {
        return match host.parse::<std::net::IpAddr>() {
            Ok(addr) => cidr_contains(network, prefix, addr),
            Err(_) => false,
        };
    }
    if let Some(suffix) = entry.strip_prefix('*') {
        let suffix = suffix.trim_start_matches('.');
        return host.ends_with(&format!(".{suffix}")) || host == suffix;
    }
    host == entry
}

/// Parse `a.b.c.d/len` (or `x::y/len`) into its base address and prefix length.
fn parse_cidr(entry: &str) -> Option<(std::net::IpAddr, u8)> {
    let (address, prefix) = entry.split_once('/')?;
    let network = address.parse::<std::net::IpAddr>().ok()?;
    let max = match network {
        std::net::IpAddr::V4(_) => 32,
        std::net::IpAddr::V6(_) => 128,
    };
    let prefix = prefix.parse::<u8>().ok()?;
    if prefix > max {
        return None;
    }
    Some((network, prefix))
}

/// True when `addr` shares the first `prefix` bits with `network`.
fn cidr_contains(network: std::net::IpAddr, prefix: u8, addr: std::net::IpAddr) -> bool {
    match (network, addr) {
        (std::net::IpAddr::V4(net), std::net::IpAddr::V4(other)) => {
            prefix > 32 || prefix_of_v4(net, prefix) == prefix_of_v4(other, prefix)
        }
        (std::net::IpAddr::V6(net), std::net::IpAddr::V6(other)) => {
            prefix > 128 || prefix_of_v6(net, prefix) == prefix_of_v6(other, prefix)
        }
        _ => false,
    }
}

/// The first `prefix` bits of an IPv4 address, zero-padded.
fn prefix_of_v4(net: std::net::Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(net);
    if prefix == 0 {
        return 0;
    }
    bits >> (32 - prefix)
}

/// The first `prefix` bits of an IPv6 address, zero-padded.
fn prefix_of_v6(net: std::net::Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(net);
    if prefix == 0 {
        return 0;
    }
    bits >> (128 - prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                name.parse::<http::HeaderName>().unwrap(),
                value.parse::<http::HeaderValue>().unwrap(),
            );
        }
        map
    }

    #[test]
    fn oversized_bodies_are_refused() {
        let policy = BodyPolicy::new(1024);
        let hs = headers(&[("content-length", "2048")]);
        let err = check_body_guards(&hs, "POST", policy).unwrap_err();
        assert_eq!(err.status(), 413);
        assert_eq!(
            err.problem_type(),
            crate::domain::gts_helpers::problem::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn chunked_is_accepted_and_identity_is_not() {
        let policy = BodyPolicy::new(1024);
        let hs = headers(&[("transfer-encoding", "chunked")]);
        assert!(check_body_guards(&hs, "POST", policy).is_ok());
        let hs = headers(&[("transfer-encoding", "identity")]);
        assert!(check_body_guards(&hs, "POST", policy).is_err());
    }

    #[test]
    fn a_malformed_content_length_is_a_framing_error() {
        let policy = BodyPolicy::new(1024);
        let hs = headers(&[("content-length", "many")]);
        let err = check_body_guards(&hs, "POST", policy).unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(
            err.problem_type(),
            crate::domain::gts_helpers::problem::VALIDATION_ERROR
        );
        // A declared body on a bodyless method is *not* a framing error: any
        // method may carry a body, per the DESIGN body-validation table.
        let hs = headers(&[("content-length", "8")]);
        assert!(check_body_guards(&hs, "GET", policy).is_ok());
    }

    #[test]
    fn ssrf_lists_are_evaluated() {
        let mut policy = SsrfPolicy::permissive();
        policy.denied_hosts = vec!["169.254.169.254".to_owned(), "*.internal".to_owned()];
        assert!(!policy.allows("169.254.169.254"));
        assert!(!policy.allows("db.internal"));
        assert!(policy.allows("api.openai.com"));
        let mut empty = SsrfPolicy::permissive();
        empty.allowed_hosts = Vec::new();
        assert!(empty.allows("anything.example.com"));
    }

    #[test]
    fn an_empty_allowlist_allows_everything_not_denied() {
        let policy = SsrfPolicy {
            enabled: true,
            allowed_hosts: vec![],
            denied_hosts: vec!["metadata.google.internal".to_owned()],
        };
        assert!(policy.allows("example.com"));
        assert!(!policy.allows("metadata.google.internal"));
    }
}
