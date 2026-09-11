//! Header handling on both legs of the proxy.
//!
//! Three categories, per `docs/DESIGN.md` §"Headers Transformation":
//! routing headers (consumed, never forwarded), hop-by-hop headers (stripped
//! per RFC 9110 §7.6.1), and passthrough headers (forwarded subject to
//! configuration).

use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{HeadersConfig, PassthroughMode};
use crate::domain::plugin::HeaderBag;

/// Headers OAGW consumes while routing and never forwards.
pub const ROUTING_HEADERS: [&str; 1] = ["x-oagw-target-host"];

/// Hop-by-hop headers, always stripped in both directions.
pub const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Inbound headers that belong to the OAGW hop itself rather than the
/// upstream exchange, and so are never forwarded.
const GATEWAY_HEADERS: [&str; 2] = ["host", "authorization"];

/// `true` when `name` must be dropped before forwarding upstream.
#[must_use]
pub fn is_stripped(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lower.as_str()) || ROUTING_HEADERS.contains(&lower.as_str())
}

/// Build the outbound header set from the inbound request.
///
/// The caller's `Authorization` header authenticates them to *OAGW*; it is
/// never relayed, because the upstream credential is the auth plugin's job
/// (`cpt-cf-oagw-nfr-credential-isolation`).
///
/// # Errors
///
/// `400` when a forwarded header name or value is not valid HTTP.
pub fn build_outbound_headers(
    inbound: &HeaderMap,
    config: &HeadersConfig,
) -> DomainResult<HeaderBag> {
    let mut bag = HeaderBag::new();
    let rules = &config.request;

    let allowlist: Vec<String> = rules
        .passthrough_allowlist
        .iter()
        .map(|n| n.to_ascii_lowercase())
        .collect();
    let removals: Vec<String> = rules.remove.iter().map(|n| n.to_ascii_lowercase()).collect();

    for (name, value) in inbound {
        let lower = name.as_str().to_ascii_lowercase();
        if is_stripped(&lower) || GATEWAY_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if removals.contains(&lower) {
            continue;
        }
        let forward = match rules.passthrough {
            PassthroughMode::None => is_content_header(&lower),
            PassthroughMode::Allowlist => is_content_header(&lower) || allowlist.contains(&lower),
            PassthroughMode::All => true,
        };
        if !forward {
            continue;
        }
        let text = value.to_str().map_err(|_| {
            DomainError::validation(format!("header '{lower}' contains non-ASCII bytes"))
        })?;
        reject_control_characters(&lower, text)?;
        bag.add(&lower, text);
    }

    // Explicit rules run last so they always win over passthrough.
    for (name, value) in &rules.set {
        reject_control_characters(name, value)?;
        bag.set(name, value.clone());
    }
    for (name, value) in &rules.add {
        reject_control_characters(name, value)?;
        bag.add(name, value.clone());
    }
    for name in &rules.remove {
        bag.remove(name);
    }

    Ok(bag)
}

/// Content negotiation and framing headers the upstream needs even under
/// `passthrough: none` — dropping them would silently corrupt request bodies.
fn is_content_header(name: &str) -> bool {
    matches!(
        name,
        "content-type" | "content-length" | "content-encoding" | "accept" | "accept-encoding"
    )
}

/// HTTP request smuggling defence: a CR or LF inside a header value splits
/// the message on a lenient upstream parser.
fn reject_control_characters(name: &str, value: &str) -> DomainResult<()> {
    if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err(DomainError::validation(format!(
            "header '{name}' contains a control character"
        )));
    }
    Ok(())
}

/// Apply the response-direction rules and strip hop-by-hop headers.
#[must_use]
pub fn apply_response_rules(mut headers: HeaderMap, config: &HeadersConfig) -> HeaderMap {
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
    let rules = &config.response;
    for name in &rules.remove {
        if let Ok(header) = HeaderName::try_from(name.to_ascii_lowercase()) {
            headers.remove(&header);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(header), Ok(value)) = (
            HeaderName::try_from(name.to_ascii_lowercase()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(header, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(header), Ok(value)) = (
            HeaderName::try_from(name.to_ascii_lowercase()),
            HeaderValue::from_str(value),
        ) {
            headers.append(header, value);
        }
    }
    headers
}

/// Convert a [`HeaderBag`] into an `http::HeaderMap`, dropping entries that
/// cannot be represented.
///
/// # Errors
///
/// `400` when a name or value is not valid HTTP.
pub fn bag_to_header_map(bag: &HeaderBag) -> DomainResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in bag.iter() {
        let header = HeaderName::try_from(name)
            .map_err(|_| DomainError::validation(format!("invalid header name '{name}'")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| DomainError::validation(format!("invalid value for header '{name}'")))?;
        map.append(header, value);
    }
    Ok(map)
}

/// Copy an `http::HeaderMap` into a [`HeaderBag`] for the plugin chain.
#[must_use]
pub fn header_map_to_bag(map: &HeaderMap) -> HeaderBag {
    let mut bag = HeaderBag::new();
    for (name, value) in map {
        if let Ok(text) = value.to_str() {
            bag.add(name.as_str(), text);
        }
    }
    bag
}

/// Validate inbound framing headers before any byte is buffered.
///
/// # Errors
///
/// * `400` when `Content-Length` is malformed or `Transfer-Encoding` names an
///   unsupported coding.
/// * `413` when the declared length exceeds `limit`.
pub fn validate_framing(headers: &HeaderMap, limit: u64) -> DomainResult<()> {
    if let Some(raw) = headers.get(http::header::CONTENT_LENGTH) {
        let text = raw
            .to_str()
            .map_err(|_| DomainError::validation("Content-Length is not valid ASCII"))?;
        let declared: u64 = text
            .trim()
            .parse()
            .map_err(|_| DomainError::validation("Content-Length is not a valid integer"))?;
        if declared > limit {
            return Err(DomainError::payload_too_large(format!(
                "request body of {declared} bytes exceeds the {limit} byte limit"
            )));
        }
    }
    if let Some(raw) = headers.get(http::header::TRANSFER_ENCODING) {
        let text = raw
            .to_str()
            .map_err(|_| DomainError::validation("Transfer-Encoding is not valid ASCII"))?;
        for coding in text.split(',') {
            let coding = coding.trim().to_ascii_lowercase();
            if !coding.is_empty() && coding != "chunked" {
                return Err(DomainError::validation(format!(
                    "unsupported Transfer-Encoding '{coding}'; only 'chunked' is supported"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::RequestHeadersConfig;
    use std::collections::BTreeMap;

    fn inbound(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::try_from(*name).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    #[test]
    fn hop_by_hop_and_routing_headers_never_reach_the_upstream() {
        let headers = inbound(&[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "Basic x"),
            ("te", "trailers"),
            ("trailer", "Expires"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("proxy-authenticate", "Basic"),
            ("x-oagw-target-host", "us.vendor.com"),
            ("host", "oagw.example.com"),
            ("x-keep", "yes"),
        ]);
        let config = HeadersConfig {
            request: RequestHeadersConfig {
                passthrough: PassthroughMode::All,
                ..RequestHeadersConfig::default()
            },
            ..HeadersConfig::default()
        };
        let bag = build_outbound_headers(&headers, &config).expect("build");
        for stripped in HOP_BY_HOP_HEADERS {
            assert!(!bag.contains(stripped), "{stripped} must be stripped");
        }
        assert!(!bag.contains("x-oagw-target-host"));
        assert!(!bag.contains("host"));
        assert_eq!(bag.get("x-keep"), Some("yes"));
    }

    #[test]
    fn the_callers_authorization_is_not_relayed() {
        let headers = inbound(&[("authorization", "Bearer caller-token")]);
        let config = HeadersConfig {
            request: RequestHeadersConfig {
                passthrough: PassthroughMode::All,
                ..RequestHeadersConfig::default()
            },
            ..HeadersConfig::default()
        };
        let bag = build_outbound_headers(&headers, &config).expect("build");
        assert!(!bag.contains("authorization"));
    }

    #[test]
    fn passthrough_none_keeps_only_content_headers() {
        let headers = inbound(&[
            ("content-type", "application/json"),
            ("x-custom", "value"),
            ("user-agent", "curl"),
        ]);
        let bag = build_outbound_headers(&headers, &HeadersConfig::default()).expect("build");
        assert_eq!(bag.get("content-type"), Some("application/json"));
        assert!(!bag.contains("x-custom"));
        assert!(!bag.contains("user-agent"));
    }

    #[test]
    fn allowlist_mode_forwards_named_headers_only() {
        let headers = inbound(&[("x-allowed", "1"), ("x-denied", "2")]);
        let config = HeadersConfig {
            request: RequestHeadersConfig {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["X-Allowed".to_owned()],
                ..RequestHeadersConfig::default()
            },
            ..HeadersConfig::default()
        };
        let bag = build_outbound_headers(&headers, &config).expect("build");
        assert_eq!(bag.get("x-allowed"), Some("1"));
        assert!(!bag.contains("x-denied"));
    }

    #[test]
    fn set_add_and_remove_are_applied_in_that_order() {
        let headers = inbound(&[("x-existing", "inbound")]);
        let mut set = BTreeMap::new();
        set.insert("x-existing".to_owned(), "override".to_owned());
        let mut add = BTreeMap::new();
        add.insert("x-multi".to_owned(), "one".to_owned());
        let config = HeadersConfig {
            request: RequestHeadersConfig {
                passthrough: PassthroughMode::All,
                set,
                add,
                remove: vec!["x-drop".to_owned()],
                ..RequestHeadersConfig::default()
            },
            ..HeadersConfig::default()
        };
        let bag = build_outbound_headers(&headers, &config).expect("build");
        assert_eq!(bag.get("x-existing"), Some("override"));
        assert_eq!(bag.get("x-multi"), Some("one"));
        assert!(!bag.contains("x-drop"));
    }

    #[test]
    fn control_characters_are_rejected() {
        let mut set = BTreeMap::new();
        set.insert("x-evil".to_owned(), "a\r\nInjected: yes".to_owned());
        let config = HeadersConfig {
            request: RequestHeadersConfig {
                set,
                ..RequestHeadersConfig::default()
            },
            ..HeadersConfig::default()
        };
        let err = build_outbound_headers(&HeaderMap::new(), &config).expect_err("rejected");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn response_rules_strip_and_rewrite() {
        let mut set = BTreeMap::new();
        set.insert("x-added".to_owned(), "1".to_owned());
        let config = HeadersConfig {
            response: crate::domain::model::ResponseHeadersConfig {
                set,
                remove: vec!["x-internal".to_owned()],
                ..crate::domain::model::ResponseHeadersConfig::default()
            },
            ..HeadersConfig::default()
        };
        let upstream = inbound(&[
            ("x-internal", "leak"),
            ("connection", "close"),
            ("content-type", "application/json"),
        ]);
        let out = apply_response_rules(upstream, &config);
        assert!(!out.contains_key("x-internal"));
        assert!(!out.contains_key("connection"));
        assert_eq!(out["x-added"], "1");
        assert_eq!(out["content-type"], "application/json");
    }

    #[test]
    fn framing_validation_catches_bad_length_and_encoding() {
        assert!(validate_framing(&inbound(&[("content-length", "10")]), 100).is_ok());
        assert_eq!(
            validate_framing(&inbound(&[("content-length", "not-a-number")]), 100)
                .expect_err("bad length")
                .status(),
            400
        );
        assert_eq!(
            validate_framing(&inbound(&[("content-length", "1000")]), 100)
                .expect_err("too large")
                .status(),
            413
        );
        assert!(validate_framing(&inbound(&[("transfer-encoding", "chunked")]), 100).is_ok());
        assert_eq!(
            validate_framing(&inbound(&[("transfer-encoding", "gzip")]), 100)
                .expect_err("unsupported")
                .status(),
            400
        );
    }

    #[test]
    fn bag_round_trips_through_a_header_map() {
        let mut bag = HeaderBag::new();
        bag.set("x-one", "1");
        bag.add("x-many", "a");
        bag.add("x-many", "b");
        let map = bag_to_header_map(&bag).expect("convert");
        assert_eq!(map.get_all("x-many").iter().count(), 2);
        let round_tripped = header_map_to_bag(&map);
        assert_eq!(round_tripped.get("x-one"), Some("1"));
    }
}
