//! Request/response header handling for the proxy path.

use http::HeaderMap;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::gts_helpers::TARGET_HOST_HEADER;
use crate::domain::model::{Endpoint, HeaderRules, PassthroughMode, Upstream};

/// Headers stripped from both directions per DESIGN "Headers Transformation".
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers consumed by OAGW and never forwarded upstream.
pub const ROUTING_HEADERS: &[&str] = &[TARGET_HOST_HEADER, "host"];

/// Removes hop-by-hop headers plus every named in `Connection`.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let mut extra: Vec<String> = Vec::new();
    if let Some(value) = headers.get("connection") {
        for token in value.to_str().unwrap_or_default().split(',') {
            let token = token.trim();
            if !token.is_empty() {
                extra.push(token.to_ascii_lowercase());
            }
        }
    }
    for name in HOP_BY_HOP_HEADERS
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
    {
        while headers.remove(name).is_some() {}
    }
}

/// Removes routing headers OAGW consumed.
pub fn strip_routing_headers(headers: &mut HeaderMap) {
    for name in ROUTING_HEADERS {
        while headers.remove(*name).is_some() {}
    }
}

/// `true` when the request asks for a protocol upgrade (`Connection: Upgrade`).
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
}

/// [`strip_hop_by_hop`] that preserves `Connection`/`Upgrade` so a proxy-to-
/// upstream protocol upgrade can still be negotiated.
pub fn strip_hop_by_hop_keep_upgrade(headers: &mut HeaderMap) {
    let upgrade = headers.get("upgrade").cloned();
    let connection = headers.get("connection").cloned();
    strip_hop_by_hop(headers);
    if let Some(value) = upgrade {
        headers.insert("upgrade", value);
    }
    if let Some(value) = connection {
        headers.insert("connection", value);
    }
}

/// Resolves which endpoint of the pool the request should target.
///
/// Behaviour matrix (ADR 0001 Appendix A):
///
/// * single endpoint pool → that endpoint, any `X-OAGW-Target-Host` is ignored
///   and stripped;
/// * multi-endpoint pool with a *specific* alias (not the common suffix) → the
///   pool is used as-is, the header is stripped;
/// * multi-endpoint pool with a common-suffix alias → the header is required.
pub fn resolve_target_host<'a>(
    upstream: &'a Upstream,
    target_host: Option<&str>,
) -> Result<&'a Endpoint, DomainError> {
    let endpoints = &upstream.server.endpoints;
    if endpoints.len() <= 1 {
        return Ok(&endpoints[0]);
    }
    let hosts: std::collections::BTreeSet<&str> =
        endpoints.iter().map(|e| e.host.as_str()).collect();
    if hosts.len() == 1 {
        // Same host repeated (e.g. for redundancy): no disambiguation needed.
        return Ok(&endpoints[0]);
    }
    let alias_matches_suffix = upstream
        .alias
        .split_once(':')
        .map_or(upstream.alias.as_str(), |(host, _)| host);
    let is_common_suffix_alias = hosts
        .iter()
        .all(|host| alias_matches_suffix.is_suffix_of_host(host));
    if !is_common_suffix_alias {
        return Ok(&endpoints[0]);
    }

    let Some(value) = target_host else {
        let mut valid: Vec<&str> = hosts.into_iter().collect();
        valid.sort_unstable();
        return Err(DomainError::new(
            ErrorKind::MissingTargetHost,
            format!(
                "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. Valid hosts: [{}]",
                valid.join(", ")
            ),
        )
        .with_extension("alias", serde_json::json!(upstream.alias))
        .with_extension("valid_hosts", serde_json::json!(valid)));
    };

    let candidate = value.trim();
    if !is_valid_target_host(candidate) {
        return Err(DomainError::new(
            ErrorKind::InvalidTargetHost,
            "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or special characters)".to_string(),
        )
        .with_extension("invalid_value", serde_json::json!(candidate)));
    }
    let matching: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| e.host.eq_ignore_ascii_case(candidate))
        .collect();
    if matching.is_empty() {
        let mut valid: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
        valid.sort_unstable();
        return Err(DomainError::new(
            ErrorKind::UnknownTargetHost,
            format!(
                "X-OAGW-Target-Host '{candidate}' does not match any configured endpoint. Valid hosts: [{}]",
                valid.join(", ")
            ),
        )
        .with_extension("invalid_value", serde_json::json!(candidate))
        .with_extension("valid_hosts", serde_json::json!(valid)));
    }
    Ok(matching[0])
}

trait SuffixOf {
    fn is_suffix_of_host(&self, host: &str) -> bool;
}

impl SuffixOf for &str {
    fn is_suffix_of_host(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(self)
            || host
                .to_ascii_lowercase()
                .strip_suffix(&format!(".{self}"))
                .is_some()
    }
}

/// RFC 1123 / IP-literal validation for the `X-OAGW-Target-Host` value.
#[must_use]
pub fn is_valid_target_host(value: &str) -> bool {
    if value.is_empty() || value.contains(':') && !value.starts_with('[') {
        return false;
    }
    if value.contains('/') || value.contains('\\') || value.contains('@') {
        return false;
    }
    crate::domain::alias::validate_hostname(value).is_ok()
}

/// Applies `set`/`add`/`remove` rules to a header map.
pub fn apply_header_rules(headers: &mut HeaderMap, rules: &HeaderRules) {
    for name in &rules.remove {
        while headers.remove(name.as_str()).is_some() {}
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Narrows the forwarded set to the configured passthrough policy.
///
/// An unconfigured policy (`passthrough` absent) forwards everything that
/// survived the hop-by-hop and routing strips — an operator who never
/// mentions `passthrough` expects transparent proxying.
pub fn apply_passthrough_policy(headers: &mut HeaderMap, rules: &HeaderRules) {
    let Some(mode) = rules.passthrough else {
        return;
    };
    match mode {
        PassthroughMode::All => {}
        PassthroughMode::Allowlist | PassthroughMode::None => {
            let mut kept: Vec<(String, String)> = Vec::new();
            for (name, value) in pairs_from_headers(headers) {
                if rules.forwards(&name) {
                    kept.push((name, value));
                }
            }
            headers.clear();
            for (name, value) in kept {
                if let (Ok(name), Ok(value)) = (
                    http::HeaderName::from_bytes(name.as_bytes()),
                    http::HeaderValue::from_str(&value),
                ) {
                    headers.append(name, value);
                }
            }
        }
    }
}

/// Parses a query string into ordered name/value pairs.
#[must_use]
pub fn pairs_from_query(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

/// Rebuilds an outbound header map from the plugin-visible request context.
#[must_use]
pub fn headers_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            map.append(name, value);
        }
    }
    map
}

/// Converts a header map to the plugin-visible ordered pairs.
#[must_use]
pub fn pairs_from_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme, PassthroughMode, ServerConfig};

    fn upstream(endpoints: Vec<Endpoint>, alias: &str) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            enabled: true,
            alias: alias.to_owned(),
            tags: Default::default(),
            server: ServerConfig { endpoints },
            protocol: crate::domain::model::Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ep(host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Http,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn hop_by_hop_headers_are_removed() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "keep-alive, x-private".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("x-oagw-target-host", "us.vendor.com".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        strip_hop_by_hop(&mut headers);
        strip_routing_headers(&mut headers);
        assert!(headers.get("connection").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get("x-private").is_none());
        assert!(headers.get("x-oagw-target-host").is_none());
        assert!(headers.get("accept").is_some());
    }

    #[test]
    fn single_endpoint_pool_needs_no_target_host() {
        let up = upstream(vec![ep("api.example", None)], "api.example");
        assert_eq!(
            resolve_target_host(&up, None).expect("ok").host,
            "api.example"
        );
    }

    #[test]
    fn common_suffix_alias_requires_target_host() {
        let up = upstream(
            vec![ep("us.vendor.com", None), ep("eu.vendor.com", None)],
            "vendor.com",
        );
        let err = resolve_target_host(&up, None).expect_err("missing");
        assert_eq!(err.kind, ErrorKind::MissingTargetHost);
        assert_eq!(
            err.to_problem_json()["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
        assert!(err.to_problem_json()["valid_hosts"].as_array().is_some());

        let bad = resolve_target_host(&up, Some("us.vendor.com:8443")).expect_err("invalid");
        assert_eq!(bad.kind, ErrorKind::InvalidTargetHost);

        let unknown = resolve_target_host(&up, Some("apac.vendor.com")).expect_err("unknown");
        assert_eq!(unknown.kind, ErrorKind::UnknownTargetHost);

        assert_eq!(
            resolve_target_host(&up, Some("US.Vendor.com"))
                .expect("ok")
                .host,
            "us.vendor.com"
        );
    }

    #[test]
    fn specific_alias_pool_does_not_need_target_host() {
        let up = upstream(
            vec![ep("us.vendor.com", None), ep("eu.vendor.com", None)],
            "my-pool",
        );
        assert_eq!(
            resolve_target_host(&up, None).expect("ok").host,
            "us.vendor.com"
        );
    }

    #[test]
    fn header_rules_apply_in_order_remove_add_set() {
        let mut headers = HeaderMap::new();
        headers.insert("x-drop", "1".parse().unwrap());
        headers.insert("x-set", "old".parse().unwrap());
        let rules = HeaderRules {
            remove: vec!["x-drop".to_owned()],
            add: vec![("x-add".to_owned(), "a".to_owned())]
                .into_iter()
                .collect(),
            set: vec![("x-set".to_owned(), "new".to_owned())]
                .into_iter()
                .collect(),
            passthrough: Some(PassthroughMode::None),
            ..HeaderRules::default()
        };
        apply_header_rules(&mut headers, &rules);
        assert!(headers.get("x-drop").is_none());
        assert_eq!(headers.get("x-set"), Some(&"new".parse().unwrap()));
        assert_eq!(headers.get("x-add"), Some(&"a".parse().unwrap()));
    }

    #[test]
    fn target_host_validation_rejects_specials() {
        assert!(is_valid_target_host("us.vendor.com"));
        assert!(is_valid_target_host("10.0.0.1"));
        assert!(!is_valid_target_host("us.vendor.com:8443"));
        assert!(!is_valid_target_host("a/b"));
        assert!(!is_valid_target_host(""));
        assert!(!is_valid_target_host("-bad-"));
    }
}
