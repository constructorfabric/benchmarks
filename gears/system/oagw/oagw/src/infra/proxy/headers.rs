//! Header transformation (`cpt-cf-oagw-fr-header-transform`).
//!
//! Three categories, in this order: routing headers are consumed and dropped,
//! hop-by-hop headers are stripped per RFC 9110, and the remainder is
//! forwarded according to the upstream's `passthrough` policy — which
//! defaults to `none`, so an inbound header reaches a third party only when
//! an operator opted it in.

use http::header::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::error::OagwError;
use crate::domain::model::{PassthroughMode, RequestHeaderRules, ResponseHeaderRules};

/// Hop-by-hop headers, stripped in both directions.
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

/// Headers OAGW consumes during routing, or that would leak platform-internal
/// credentials to a third party. Never forwarded, whatever the passthrough
/// mode says.
pub const NEVER_FORWARDED: &[&str] = &[
    // The inbound bearer token authenticates the caller *to OAGW*; forwarding
    // it would hand the platform's credential to the upstream service.
    "authorization",
    "proxy-authorization",
    "host",
    "content-length",
    "x-oagw-target-host",
    "x-oagw-error-source",
    "x-oagw-response-mode",
];

/// WebSocket / upgrade handshake headers that must survive stripping when the
/// request is an upgrade, because they *are* the protocol negotiation.
pub const UPGRADE_HANDSHAKE: &[&str] = &[
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

fn is_listed(list: &[&str], name: &HeaderName) -> bool {
    list.contains(&name.as_str())
}

/// Build the outbound request header set.
///
/// `inbound` is the client's header map; `rules` is the upstream's
/// `headers.request` block (absent means "the schema defaults", i.e.
/// `passthrough: none`).
///
/// # Errors
///
/// Returns `400` when a configured header name or value is not legal on the
/// wire — an invalid header must not be silently dropped.
pub fn build_request_headers(
    inbound: &HeaderMap,
    rules: Option<&RequestHeaderRules>,
    is_upgrade: bool,
) -> Result<HeaderMap, OagwError> {
    let mut out = HeaderMap::new();
    let mode = rules.map_or(PassthroughMode::None, |r| r.passthrough);
    let allowlist: Vec<String> = rules
        .map(|r| {
            r.passthrough_allowlist
                .iter()
                .map(|n| n.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    for (name, value) in inbound {
        if is_listed(NEVER_FORWARDED, name) {
            continue;
        }
        let handshake = is_upgrade && is_listed(UPGRADE_HANDSHAKE, name);
        if is_listed(HOP_BY_HOP, name) && !handshake {
            continue;
        }
        let forward = handshake
            || match mode {
                PassthroughMode::All => true,
                PassthroughMode::Allowlist => allowlist.iter().any(|a| a == name.as_str()),
                PassthroughMode::None => false,
            };
        if forward {
            out.append(name.clone(), value.clone());
        }
    }

    // `Content-Type` describes the body OAGW is forwarding, so it travels with
    // it regardless of the passthrough policy.
    if let Some(content_type) = inbound.get(http::header::CONTENT_TYPE)
        && !out.contains_key(http::header::CONTENT_TYPE)
    {
        out.insert(http::header::CONTENT_TYPE, content_type.clone());
    }

    if let Some(rules) = rules {
        for name in &rules.remove {
            if let Ok(name) = parse_name(name) {
                out.remove(&name);
            }
        }
        for (name, value) in &rules.set {
            let (name, value) = parse_pair(name, value)?;
            out.insert(name, value);
        }
        for (name, value) in &rules.add {
            let (name, value) = parse_pair(name, value)?;
            out.append(name, value);
        }
    }

    Ok(out)
}

/// Apply the response-direction rules to the upstream's headers.
///
/// # Errors
///
/// Returns `400` when a configured header name or value is not legal.
pub fn apply_response_rules(
    headers: &mut HeaderMap,
    rules: Option<&ResponseHeaderRules>,
) -> Result<(), OagwError> {
    let Some(rules) = rules else { return Ok(()) };
    for name in &rules.remove {
        if let Ok(name) = parse_name(name) {
            headers.remove(&name);
        }
    }
    for (name, value) in &rules.set {
        let (name, value) = parse_pair(name, value)?;
        headers.insert(name, value);
    }
    for (name, value) in &rules.add {
        let (name, value) = parse_pair(name, value)?;
        headers.append(name, value);
    }
    Ok(())
}

/// Strip hop-by-hop headers from an upstream response before it is handed to
/// the client. `keep_upgrade` preserves the `101` handshake headers.
pub fn strip_hop_by_hop_response(headers: &mut HeaderMap, keep_upgrade: bool) {
    let victims: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            is_listed(HOP_BY_HOP, name) && !(keep_upgrade && is_listed(UPGRADE_HANDSHAKE, name))
        })
        .cloned()
        .collect();
    for name in victims {
        headers.remove(&name);
    }
}

fn parse_name(raw: &str) -> Result<HeaderName, OagwError> {
    raw.trim()
        .parse::<HeaderName>()
        .map_err(|_| OagwError::validation(format!("'{raw}' is not a valid header name")))
}

fn parse_pair(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), OagwError> {
    let name = parse_name(name)?;
    let value = HeaderValue::from_str(value).map_err(|_| {
        OagwError::validation(format!(
            "'{value}' is not a valid value for header '{name}'"
        ))
    })?;
    Ok((name, value))
}

/// Validate well-known body framing headers on the inbound request
/// (DESIGN "Body Validation Rules").
///
/// # Errors
///
/// * `400` — malformed `Content-Length`, or a `Transfer-Encoding` other than
///   `chunked`;
/// * `413` — declared length above `max_body_bytes`.
pub fn validate_body_framing(inbound: &HeaderMap, max_body_bytes: usize) -> Result<(), OagwError> {
    if let Some(raw) = inbound.get(http::header::CONTENT_LENGTH) {
        let text = raw
            .to_str()
            .map_err(|_| OagwError::validation("Content-Length is not valid ASCII"))?;
        let declared: usize = text
            .trim()
            .parse()
            .map_err(|_| OagwError::validation("Content-Length must be a non-negative integer"))?;
        if declared > max_body_bytes {
            return Err(OagwError::payload_too_large(format!(
                "declared Content-Length {declared} exceeds the {max_body_bytes} byte limit"
            )));
        }
    }
    if let Some(raw) = inbound.get(http::header::TRANSFER_ENCODING) {
        let text = raw
            .to_str()
            .map_err(|_| OagwError::validation("Transfer-Encoding is not valid ASCII"))?;
        let unsupported = text
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .any(|token| !token.eq_ignore_ascii_case("chunked"));
        if unsupported {
            return Err(OagwError::validation(format!(
                "unsupported Transfer-Encoding '{text}'; only 'chunked' is supported"
            )));
        }
    }
    Ok(())
}

/// `true` when the request asks for a protocol upgrade (WebSocket and
/// friends): `Connection: upgrade` plus an `Upgrade` token.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let connection_upgrade = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    connection_upgrade && headers.contains_key(http::header::UPGRADE)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_response_rules, build_request_headers, is_upgrade_request, strip_hop_by_hop_response,
        validate_body_framing,
    };
    use crate::domain::model::{PassthroughMode, RequestHeaderRules, ResponseHeaderRules};
    use http::header::{HeaderMap, HeaderName, HeaderValue};
    use std::collections::BTreeMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    #[test]
    fn passthrough_defaults_to_forwarding_nothing() {
        let inbound = headers(&[("x-custom", "v"), ("accept", "application/json")]);
        let out = build_request_headers(&inbound, None, false).expect("built");
        assert!(
            out.is_empty(),
            "with no headers config nothing is forwarded, got {out:?}"
        );
    }

    #[test]
    fn content_type_always_travels_with_the_body() {
        let inbound = headers(&[("content-type", "application/json"), ("x-custom", "v")]);
        let out = build_request_headers(&inbound, None, false).expect("built");
        assert_eq!(out["content-type"], "application/json");
        assert!(!out.contains_key("x-custom"));
    }

    #[test]
    fn passthrough_all_forwards_but_never_the_inbound_bearer() {
        let inbound = headers(&[
            ("authorization", "Bearer platform-token"),
            ("x-custom", "v"),
            ("host", "oagw.example.com"),
            ("x-oagw-target-host", "us.vendor.com"),
        ]);
        let rules = RequestHeaderRules {
            passthrough: PassthroughMode::All,
            ..RequestHeaderRules::default()
        };
        let out = build_request_headers(&inbound, Some(&rules), false).expect("built");
        assert_eq!(out["x-custom"], "v");
        assert!(
            !out.contains_key("authorization"),
            "the platform bearer token must never reach an upstream"
        );
        assert!(
            !out.contains_key("host"),
            "Host is replaced by the upstream"
        );
        assert!(
            !out.contains_key("x-oagw-target-host"),
            "routing headers are consumed, not forwarded"
        );
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let inbound = headers(&[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("proxy-authenticate", "Basic"),
            ("x-keep", "v"),
        ]);
        let rules = RequestHeaderRules {
            passthrough: PassthroughMode::All,
            ..RequestHeaderRules::default()
        };
        let out = build_request_headers(&inbound, Some(&rules), false).expect("built");
        assert_eq!(out.len(), 1);
        assert_eq!(out["x-keep"], "v");
    }

    #[test]
    fn upgrade_handshake_headers_survive_for_an_upgrade_request() {
        let inbound = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ]);
        // Even with the default `none` policy the handshake must be forwarded,
        // or the upgrade cannot be negotiated at all.
        let out = build_request_headers(&inbound, None, true).expect("built");
        assert_eq!(out["upgrade"], "websocket");
        assert_eq!(out["sec-websocket-version"], "13");
        assert!(out.contains_key("connection"));
    }

    #[test]
    fn allowlist_mode_forwards_only_named_headers() {
        let inbound = headers(&[("x-a", "1"), ("x-b", "2")]);
        let rules = RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: vec!["X-A".to_owned()],
            ..RequestHeaderRules::default()
        };
        let out = build_request_headers(&inbound, Some(&rules), false).expect("built");
        assert_eq!(out["x-a"], "1");
        assert!(!out.contains_key("x-b"));
    }

    #[test]
    fn set_add_and_remove_are_applied_in_order() {
        let inbound = headers(&[("x-drop", "1"), ("x-keep", "2")]);
        let mut set = BTreeMap::new();
        set.insert("x-keep".to_owned(), "overwritten".to_owned());
        let mut add = BTreeMap::new();
        add.insert("x-multi".to_owned(), "a".to_owned());
        let rules = RequestHeaderRules {
            set,
            add,
            remove: vec!["X-Drop".to_owned()],
            passthrough: PassthroughMode::All,
            passthrough_allowlist: Vec::new(),
        };
        let out = build_request_headers(&inbound, Some(&rules), false).expect("built");
        assert!(!out.contains_key("x-drop"));
        assert_eq!(out["x-keep"], "overwritten");
        assert_eq!(out["x-multi"], "a");
    }

    #[test]
    fn an_illegal_configured_header_is_a_validation_error() {
        let mut set = BTreeMap::new();
        set.insert("bad header".to_owned(), "v".to_owned());
        let rules = RequestHeaderRules {
            set,
            ..RequestHeaderRules::default()
        };
        let err =
            build_request_headers(&HeaderMap::new(), Some(&rules), false).expect_err("rejected");
        assert_eq!(err.status, 400);

        let mut set = BTreeMap::new();
        set.insert("x-ok".to_owned(), "bad\nvalue".to_owned());
        let rules = RequestHeaderRules {
            set,
            ..RequestHeaderRules::default()
        };
        assert!(build_request_headers(&HeaderMap::new(), Some(&rules), false).is_err());
    }

    #[test]
    fn response_rules_are_applied() {
        let mut headers = headers(&[("x-upstream", "1"), ("x-secret", "2")]);
        let mut set = BTreeMap::new();
        set.insert("x-upstream".to_owned(), "rewritten".to_owned());
        let rules = ResponseHeaderRules {
            set,
            add: BTreeMap::new(),
            remove: vec!["x-secret".to_owned()],
        };
        apply_response_rules(&mut headers, Some(&rules)).expect("applied");
        assert_eq!(headers["x-upstream"], "rewritten");
        assert!(!headers.contains_key("x-secret"));
    }

    #[test]
    fn response_hop_by_hop_stripping_respects_upgrades() {
        let mut plain = headers(&[("connection", "keep-alive"), ("content-type", "text/plain")]);
        strip_hop_by_hop_response(&mut plain, false);
        assert!(!plain.contains_key("connection"));
        assert!(plain.contains_key("content-type"));

        let mut upgraded = headers(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        strip_hop_by_hop_response(&mut upgraded, true);
        assert!(upgraded.contains_key("connection"));
        assert!(upgraded.contains_key("upgrade"));
    }

    #[test]
    fn body_framing_validation() {
        validate_body_framing(&headers(&[("content-length", "10")]), 1024).expect("ok");
        assert_eq!(
            validate_body_framing(&headers(&[("content-length", "abc")]), 1024)
                .expect_err("malformed")
                .status,
            400
        );
        assert_eq!(
            validate_body_framing(&headers(&[("content-length", "-1")]), 1024)
                .expect_err("negative")
                .status,
            400
        );
        assert_eq!(
            validate_body_framing(&headers(&[("content-length", "2048")]), 1024)
                .expect_err("too large")
                .status,
            413
        );
        validate_body_framing(&headers(&[("transfer-encoding", "chunked")]), 1024)
            .expect("chunked is supported");
        assert_eq!(
            validate_body_framing(&headers(&[("transfer-encoding", "gzip, chunked")]), 1024)
                .expect_err("unsupported")
                .status,
            400
        );
    }

    #[test]
    fn upgrade_detection() {
        assert!(is_upgrade_request(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket")
        ])));
        assert!(is_upgrade_request(&headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket")
        ])));
        assert!(!is_upgrade_request(&headers(&[("upgrade", "websocket")])));
        assert!(!is_upgrade_request(&headers(&[(
            "connection",
            "keep-alive"
        )])));
    }
}
