//! Header transformation for the proxy path.
//!
//! `cpt-cf-oagw-fr-header-transform` splits inbound headers into three
//! categories: routing headers OAGW consumes and does not forward, hop-by-hop
//! headers stripped per RFC 9110, and passthrough headers governed by the
//! upstream's `headers` configuration.
//!
//! One rule is not in the schema but follows from
//! `cpt-cf-oagw-nfr-credential-isolation`: the caller's inbound
//! `Authorization` (the platform bearer token) is never forwarded to an
//! external service. Outbound credentials come from the auth plugin, which
//! runs after this transformation.

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};

use crate::domain::error::TARGET_HOST_HEADER;
use crate::domain::model::{HeadersConfig, Passthrough, ResponseHeadersConfig};

/// Headers that are meaningful only for a single hop.
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

/// Headers OAGW consumes during routing and never forwards.
pub const ROUTING_HEADERS: &[&str] = &[TARGET_HOST_HEADER, "x-oagw-response-mode"];

/// Prefixes of headers the platform's own edge adds to an inbound request.
/// They describe the *inbound* gateway's state and mean nothing to an
/// external service, so they are stripped rather than leaked
/// (`cpt-cf-oagw-nfr-ssrf-protection`: "strip well-known internal headers").
const INTERNAL_PREFIXES: &[&str] = &["x-oagw-", "ratelimit-", "x-ratelimit-"];

/// Representation metadata that describes the body being forwarded, and so
/// travels with it regardless of the passthrough policy.
const REPRESENTATION_HEADERS: &[&str] = &["content-type", "accept"];

/// Whether a header name must never cross the gateway boundary outbound.
#[must_use]
pub fn is_stripped_outbound(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
        || ROUTING_HEADERS.contains(&lower.as_str())
        || INTERNAL_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
        || lower == "host"
        || lower == "content-length"
        // The platform bearer belongs to the platform, not to the upstream.
        || lower == "authorization"
        || lower == "cookie"
}

/// Whether a header name must be dropped from an upstream response.
#[must_use]
pub fn is_stripped_inbound(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str()) || lower == "content-length"
}

/// Build the outbound header set from the inbound one.
///
/// Order matters: passthrough selection, then `remove`, then `set`
/// (overwriting), then `add` (appending). A `set`/`add` rule therefore always
/// wins over what the client sent.
#[must_use]
pub fn build_outbound_headers(inbound: &HeaderMap, config: &HeadersConfig) -> HeaderMap {
    let request_config = config.request.clone().unwrap_or_default();
    let passthrough = request_config.effective_passthrough();
    let allowlist: Vec<String> = request_config
        .passthrough_allowlist
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let removals: Vec<String> = request_config
        .remove
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();

    let mut outbound = HeaderMap::new();
    for (name, value) in inbound {
        let lower = name.as_str().to_ascii_lowercase();
        if is_stripped_outbound(&lower) {
            continue;
        }
        if removals.contains(&lower) {
            continue;
        }
        let forwarded = match passthrough {
            Passthrough::All => true,
            Passthrough::Allowlist => {
                allowlist.contains(&lower) || REPRESENTATION_HEADERS.contains(&lower.as_str())
            }
            Passthrough::None => REPRESENTATION_HEADERS.contains(&lower.as_str()),
        };
        if forwarded {
            outbound.append(name.clone(), value.clone());
        }
    }

    for (name, value) in &request_config.set {
        if let (Ok(name), Ok(value)) = (parse_name(name), HeaderValue::from_str(value)) {
            outbound.insert(name, value);
        }
    }
    for (name, value) in &request_config.add {
        if let (Ok(name), Ok(value)) = (parse_name(name), HeaderValue::from_str(value)) {
            outbound.append(name, value);
        }
    }
    outbound
}

/// Apply the response-side rules to headers coming back from the upstream.
#[must_use]
pub fn build_response_headers(
    upstream: &HeaderMap,
    config: Option<&ResponseHeadersConfig>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    let removals: Vec<String> = config
        .map(|c| c.remove.iter().map(|n| n.to_ascii_lowercase()).collect())
        .unwrap_or_default();
    for (name, value) in upstream {
        let lower = name.as_str().to_ascii_lowercase();
        if is_stripped_inbound(&lower) || removals.contains(&lower) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Some(config) = config {
        for (name, value) in &config.set {
            if let (Ok(name), Ok(value)) = (parse_name(name), HeaderValue::from_str(value)) {
                out.insert(name, value);
            }
        }
        for (name, value) in &config.add {
            if let (Ok(name), Ok(value)) = (parse_name(name), HeaderValue::from_str(value)) {
                out.append(name, value);
            }
        }
    }
    out
}

/// Parse a configured header name, lowercased.
fn parse_name(raw: &str) -> Result<HeaderName, axum::http::header::InvalidHeaderName> {
    HeaderName::try_from(raw.trim().to_ascii_lowercase())
}

/// Set the `Host` header (HTTP/1.1) / `:authority` (HTTP/2) to the upstream
/// authority, per `DESIGN.md` § *Headers Transformation*.
pub fn set_host(headers: &mut HeaderMap, authority: &str) {
    if let Ok(value) = HeaderValue::from_str(authority) {
        headers.insert(header::HOST, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::RequestHeadersConfig;
    use std::collections::BTreeMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn hop_by_hop_and_routing_headers_are_stripped() {
        let inbound = headers(&[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("proxy-authenticate", "Basic"),
            ("proxy-authorization", "Basic x"),
            ("te", "trailers"),
            ("trailer", "Expires"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("host", "oagw.example.com"),
            ("x-oagw-target-host", "us.vendor.com"),
            ("authorization", "Bearer platform-token"),
            ("x-keep-me", "yes"),
        ]);
        let outbound = build_outbound_headers(&inbound, &HeadersConfig::default());
        for name in HOP_BY_HOP {
            assert!(outbound.get(*name).is_none(), "{name} must be stripped");
        }
        assert!(outbound.get("host").is_none());
        assert!(outbound.get("x-oagw-target-host").is_none());
        assert!(
            outbound.get("authorization").is_none(),
            "the platform bearer must not reach the upstream"
        );
        assert_eq!(outbound.get("x-keep-me").unwrap(), "yes");
    }

    #[test]
    fn absent_config_is_a_transparent_proxy() {
        let inbound = headers(&[("x-custom", "v"), ("content-type", "application/json")]);
        let outbound = build_outbound_headers(&inbound, &HeadersConfig::default());
        assert_eq!(outbound.get("x-custom").unwrap(), "v");
        assert_eq!(outbound.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn passthrough_none_keeps_only_representation_metadata() {
        let config = HeadersConfig {
            request: Some(RequestHeadersConfig {
                passthrough: Some(Passthrough::None),
                ..RequestHeadersConfig::default()
            }),
            response: None,
        };
        let inbound = headers(&[("x-custom", "v"), ("content-type", "application/json")]);
        let outbound = build_outbound_headers(&inbound, &config);
        assert!(outbound.get("x-custom").is_none());
        assert_eq!(
            outbound.get("content-type").unwrap(),
            "application/json",
            "the body's own media type still describes the body"
        );
    }

    #[test]
    fn passthrough_allowlist_forwards_only_named_headers() {
        let config = HeadersConfig {
            request: Some(RequestHeadersConfig {
                passthrough: Some(Passthrough::Allowlist),
                passthrough_allowlist: vec!["X-Keep".to_owned()],
                ..RequestHeadersConfig::default()
            }),
            response: None,
        };
        let inbound = headers(&[("x-keep", "yes"), ("x-drop", "no")]);
        let outbound = build_outbound_headers(&inbound, &config);
        assert_eq!(outbound.get("x-keep").unwrap(), "yes");
        assert!(outbound.get("x-drop").is_none());
    }

    #[test]
    fn set_overwrites_add_appends_remove_drops() {
        let config = HeadersConfig {
            request: Some(RequestHeadersConfig {
                set: map(&[("x-set", "fixed")]),
                add: map(&[("x-add", "extra")]),
                remove: vec!["X-Drop".to_owned()],
                passthrough: Some(Passthrough::All),
                passthrough_allowlist: vec![],
            }),
            response: None,
        };
        let inbound = headers(&[("x-set", "client"), ("x-drop", "no"), ("x-add", "client")]);
        let outbound = build_outbound_headers(&inbound, &config);
        assert_eq!(outbound.get("x-set").unwrap(), "fixed");
        assert!(outbound.get("x-drop").is_none());
        let added: Vec<_> = outbound
            .get_all("x-add")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(added, vec!["client", "extra"]);
    }

    #[test]
    fn response_rules_strip_set_and_add() {
        let config = ResponseHeadersConfig {
            set: map(&[("x-set", "fixed")]),
            add: map(&[("x-add", "extra")]),
            remove: vec!["X-Secret".to_owned()],
        };
        let upstream = headers(&[
            ("x-secret", "leak"),
            ("x-set", "upstream"),
            ("content-type", "application/json"),
            ("transfer-encoding", "chunked"),
        ]);
        let out = build_response_headers(&upstream, Some(&config));
        assert!(out.get("x-secret").is_none());
        assert_eq!(out.get("x-set").unwrap(), "fixed");
        assert_eq!(out.get("x-add").unwrap(), "extra");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(
            out.get("transfer-encoding").is_none(),
            "hop-by-hop headers do not survive the hop"
        );
    }

    #[test]
    fn response_without_config_passes_through() {
        let upstream = headers(&[("x-upstream", "v")]);
        let out = build_response_headers(&upstream, None);
        assert_eq!(out.get("x-upstream").unwrap(), "v");
    }

    #[test]
    fn host_is_replaced_by_the_upstream_authority() {
        let mut map = headers(&[("host", "oagw.example.com")]);
        set_host(&mut map, "api.openai.com:443");
        assert_eq!(map.get("host").unwrap(), "api.openai.com:443");
    }

    #[test]
    fn platform_edge_headers_are_not_leaked_outbound() {
        let inbound = headers(&[
            ("ratelimit-limit", "200"),
            ("x-ratelimit-remaining", "199"),
            ("ratelimit-policy", "\"burst\";q=200;w=1000"),
            ("x-oagw-response-mode", "envelope"),
            ("x-request-id", "req_abc"),
        ]);
        let outbound = build_outbound_headers(&inbound, &HeadersConfig::default());
        for name in [
            "ratelimit-limit",
            "x-ratelimit-remaining",
            "ratelimit-policy",
            "x-oagw-response-mode",
        ] {
            assert!(outbound.get(name).is_none(), "{name} must not be forwarded");
        }
        assert_eq!(
            outbound.get("x-request-id").unwrap(),
            "req_abc",
            "the correlation id is not an internal header"
        );
    }

    #[test]
    fn multi_valued_headers_survive() {
        let inbound = headers(&[("accept-encoding", "gzip"), ("accept-encoding", "br")]);
        let outbound = build_outbound_headers(&inbound, &HeadersConfig::default());
        let values: Vec<_> = outbound
            .get_all("accept-encoding")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(values, vec!["gzip", "br"]);
    }
}
