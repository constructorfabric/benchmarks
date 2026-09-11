//! Header treatment for both proxy legs (`contracts/proxy-api.md` § 2).

use http::HeaderMap;
use std::collections::BTreeMap;

use crate::domain::model::{HeaderRules, PassthroughMode};
use crate::infra::ratelimit::RateLimitDecision;

/// Hop-by-hop headers, plus the gateway-reserved ones (`RFC 9110` § 7.6.1 and
/// `DESIGN.md` § 3.3).
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers the gateway consumes itself and never forwards.
/// Header a caller may set to pin the target host of an upstream with
/// several endpoints.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

pub const GATEWAY_RESERVED: &[&str] = &[TARGET_HOST_HEADER];

/// Header applied to every proxied response.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Builds the outbound request headers from the inbound ones.
///
/// Order: strip hop-by-hop and gateway-reserved entries, apply the
/// passthrough mode, then `headers.request.set` and `headers.request.add`.
/// `Host`, `Content-Length` and `Authorization` are handled by the caller.
#[must_use]
pub fn build_request_headers(inbound: &HeaderMap, rules: Option<&HeaderRules>) -> HeaderMap {
    let mut out = HeaderMap::new();
    let rules = rules.cloned().unwrap_or_default();

    let forward_all = rules.request.passthrough == PassthroughMode::All;
    let allowlist: Vec<String> = rules
        .request
        .passthrough_allowlist
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    let forward_inbound = rules.request.passthrough != PassthroughMode::None;

    if forward_inbound {
        for (name, value) in inbound.iter() {
            let lower = name.as_str().to_ascii_lowercase();
            if is_hop_by_hop(name.as_str()) || GATEWAY_RESERVED.contains(&lower.as_str()) {
                continue;
            }
            if !forward_all && !allowlist.iter().any(|allowed| allowed == name.as_str()) {
                continue;
            }
            out.insert(name.clone(), value.clone());
        }
    }

    for name in &rules.request.remove {
        if let Ok(header_name) = http::HeaderName::try_from(name.as_str()) {
            out.remove(header_name);
        }
    }
    for (name, value) in &rules.request.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::try_from(value.as_str()),
        ) {
            out.insert(name, value);
        }
    }
    for (name, value) in &rules.request.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::try_from(value.as_str()),
        ) {
            out.append(name, value);
        }
    }
    out
}

/// Whether a header name is hop-by-hop.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

/// Builds the response headers returned to the client.
///
/// Order: strip hop-by-hop, apply `headers.response` rules, then the gateway
/// headers (`X-OAGW-Error-Source`, `X-Request-ID`).
#[must_use]
pub fn build_response_headers(
    upstream: &HeaderMap,
    rules: Option<&HeaderRules>,
    error_source: &str,
    request_id: Option<&str>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    if let Some(rules) = rules {
        for name in &rules.response.remove {
            if let Ok(header_name) = http::HeaderName::try_from(name.as_str()) {
                out.remove(header_name);
            }
        }
        for (name, value) in &rules.response.set {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(value.as_str()),
            ) {
                out.insert(name, value);
            }
        }
        for (name, value) in &rules.response.add {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(value.as_str()),
            ) {
                out.append(name, value);
            }
        }
    }
    if let Ok(value) = http::HeaderValue::from_str(error_source) {
        out.insert(ERROR_SOURCE_HEADER, value);
    }
    if let Some(id) = request_id
        && let Ok(value) = http::HeaderValue::from_str(id)
    {
        out.insert("x-request-id", value);
    }
    out
}

/// Applies the CORS response headers for an allowed actual request.
pub fn apply_cors_response(
    headers: &mut HeaderMap,
    config: &crate::domain::model::Cors,
    origin: &str,
) {
    if !config.enabled {
        return;
    }
    if let Some(allowed) = crate::infra::cors::allow_origin(config, origin)
        && let Ok(value) = http::HeaderValue::from_str(&allowed)
    {
        headers.insert("access-control-allow-origin", value);
    }
    if config.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            http::HeaderValue::from_static("true"),
        );
    }
    if !config.expose_headers.is_empty()
        && let Ok(value) = http::HeaderValue::from_str(&config.expose_headers.join(", "))
    {
        headers.insert("access-control-expose-headers", value);
    }
    headers.insert(http::header::VARY, http::HeaderValue::from_static("Origin"));
}

/// Restores the upgrade headers a `101` carries.
///
/// Both are hop-by-hop and therefore stripped by the ordinary response
/// treatment, but the client cannot complete the protocol switch without them
/// (`contracts/proxy-api.md` § 3).
pub fn apply_upgrade_response(headers: &mut HeaderMap, upstream: &HeaderMap) {
    for name in [http::header::UPGRADE, http::header::CONNECTION] {
        if let Some(value) = upstream.get(&name) {
            headers.insert(&name, value.clone());
        }
    }
}

/// Writes the rate-limit exposure headers onto a response (`contracts/proxy-api.md` § 4).
pub fn apply_rate_limit(headers: &mut HeaderMap, decision: &RateLimitDecision) {
    for (name, value) in [
        ("x-ratelimit-limit", decision.limit.to_string()),
        ("x-ratelimit-remaining", decision.remaining.to_string()),
        ("x-ratelimit-reset", decision.reset_secs.to_string()),
    ] {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::try_from(name),
            http::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Applies a `set` map to a header collection (used by the error renderer).
pub fn set_headers(headers: &mut HeaderMap, entries: &BTreeMap<String, String>) {
    for (name, value) in entries {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::try_from(value.as_str()),
        ) {
            headers.insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbound() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            "Bearer caller".parse().unwrap(),
        );
        headers.insert("x-custom", "keep".parse().unwrap());
        headers.insert(http::header::CONNECTION, "keep-alive".parse().unwrap());
        headers.insert("x-oagw-target-host", "eu.vendor.com".parse().unwrap());
        headers
    }

    #[test]
    fn the_default_forwards_nothing() {
        let out = build_request_headers(&inbound(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn the_allowlist_forwards_only_listed_names() {
        let rules = HeaderRules {
            request: crate::domain::model::RequestHeaderRules {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["X-Custom".to_owned()],
                ..Default::default()
            },
            ..Default::default()
        };
        let out = build_request_headers(&inbound(), Some(&rules));
        assert!(out.contains_key("x-custom"));
        assert!(!out.contains_key(http::header::AUTHORIZATION));
        assert!(!out.contains_key("x-oagw-target-host"));
    }

    #[test]
    fn all_mode_strips_only_hop_by_hop() {
        let rules = HeaderRules {
            request: crate::domain::model::RequestHeaderRules {
                passthrough: PassthroughMode::All,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = build_request_headers(&inbound(), Some(&rules));
        assert!(out.contains_key(http::header::AUTHORIZATION));
        assert!(!out.contains_key(http::header::CONNECTION));
    }

    #[test]
    fn set_and_remove_are_applied_after_passthrough() {
        let rules = HeaderRules {
            request: crate::domain::model::RequestHeaderRules {
                passthrough: PassthroughMode::All,
                set: [
                    ("X-Replaced".to_owned(), "new".to_owned()),
                    (
                        http::header::AUTHORIZATION.to_string(),
                        "Bearer gateway".to_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                remove: vec!["X-Custom".to_owned()],
                ..Default::default()
            },
            ..Default::default()
        };
        let out = build_request_headers(&inbound(), Some(&rules));
        assert_eq!(out.get("x-replaced").unwrap(), "new");
        assert_eq!(
            out.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer gateway"
        );
        assert!(out.get("x-custom").is_none());
    }

    #[test]
    fn the_response_strips_hop_by_hop_and_tags_the_source() {
        let mut upstream = HeaderMap::new();
        upstream.insert("content-type", "application/json".parse().unwrap());
        upstream.insert("connection", "close".parse().unwrap());
        let rules = HeaderRules {
            response: crate::domain::model::ResponseHeaderRules {
                set: [("X-Stamped".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        let out = build_response_headers(&upstream, Some(&rules), "upstream", Some("abc"));
        assert!(out.contains_key("content-type"));
        assert!(!out.contains_key(http::header::CONNECTION));
        assert_eq!(out.get("x-oagw-error-source").unwrap(), "upstream");
        assert_eq!(out.get("x-request-id").unwrap(), "abc");
        assert_eq!(out.get("x-stamped").unwrap(), "1");
    }

    #[test]
    fn cors_headers_are_added_for_an_allowed_origin() {
        let config = crate::domain::model::Cors {
            enabled: true,
            allowed_origins: vec!["https://app.example".to_owned()],
            allow_credentials: true,
            expose_headers: vec!["X-Request-ID".to_owned()],
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_cors_response(&mut headers, &config, "https://app.example");
        assert_eq!(
            headers.get("access-control-allow-origin").unwrap(),
            "https://app.example"
        );
        assert_eq!(
            headers.get("access-control-allow-credentials").unwrap(),
            "true"
        );
        assert_eq!(headers.get(http::header::VARY).unwrap(), "Origin");
    }

    #[test]
    fn hop_by_hop_detection_is_case_insensitive() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(!is_hop_by_hop("content-type"));
    }
}
