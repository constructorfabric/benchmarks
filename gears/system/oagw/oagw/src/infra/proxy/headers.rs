// Created: 2026-08-29 by Constructor Tech
//! Hop-by-hop stripping and request / response header transformation rules.
//!
//! Security: header *values* are never formatted into a log line here; only
//! the rules themselves are applied.

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::model::{RequestHeaders, ResponseHeaders};

/// Headers never forwarded in either direction.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Header read and then stripped before forwarding (`X-OAGW-Target-Host`).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Header always present on gateway and upstream responses.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Build a header value, or `None` when the text is not valid.
#[must_use]
pub fn header_value(text: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(text).ok()
}

/// Build a header name, or `None` when the text is not valid.
#[must_use]
pub fn header_name(text: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(text.as_bytes()).ok()
}

/// Strip hop-by-hop headers plus every header named in `Connection`.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("close"))
        .filter_map(header_name)
        .collect();
    for name in named {
        headers.remove(&name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// Copy every inbound header named in `name` into `headers`.
fn copy_inbound(inbound: &HeaderMap, headers: &mut HeaderMap, name: &str) {
    let Some(parsed) = header_name(name) else {
        return;
    };
    for value in inbound.get_all(&parsed) {
        headers.append(parsed.clone(), value.clone());
    }
}

/// Replace `headers` with the forwardable subset of `inbound` plus the rules.
pub fn build_request_headers(
    inbound: &HeaderMap,
    rules: &RequestHeaders,
    skip: &[&str],
) -> HeaderMap {
    let mut out = HeaderMap::new();
    match rules.passthrough {
        crate::domain::model::Passthrough::None => {}
        crate::domain::model::Passthrough::Allowlist => {
            for name in &rules.passthrough_allowlist {
                copy_inbound(inbound, &mut out, name);
            }
        }
        crate::domain::model::Passthrough::All => {
            for (name, value) in inbound {
                out.insert(name.clone(), value.clone());
            }
        }
    }
    for (name, value) in &rules.set {
        if let (Some(parsed), Some(parsed_value)) = (header_name(name), header_value(value)) {
            out.insert(parsed, parsed_value);
        }
    }
    for (name, value) in &rules.add {
        if let (Some(parsed), Some(parsed_value)) = (header_name(name), header_value(value)) {
            out.append(parsed, parsed_value);
        }
    }
    for name in &rules.remove {
        if let Some(parsed) = header_name(name) {
            out.remove(&parsed);
        }
    }
    for name in skip {
        out.remove(*name);
    }
    out
}

/// Apply the response-leg header rules to the upstream response headers.
pub fn apply_response_rules(headers: &mut HeaderMap, rules: &ResponseHeaders) {
    for (name, value) in &rules.set {
        if let (Some(parsed), Some(parsed_value)) = (header_name(name), header_value(value)) {
            headers.insert(parsed, parsed_value);
        }
    }
    for (name, value) in &rules.add {
        if let (Some(parsed), Some(parsed_value)) = (header_name(name), header_value(value)) {
            headers.append(parsed, parsed_value);
        }
    }
    for name in &rules.remove {
        if let Some(parsed) = header_name(name) {
            headers.remove(&parsed);
        }
    }
}

/// Bare host part of an `X-OAGW-Target-Host` value (scheme, port, path stripped).
#[must_use]
pub fn bare_host(value: &str) -> String {
    let stripped = value.trim();
    let without_scheme = stripped
        .split_once("://")
        .map_or(stripped, |(_scheme, rest)| rest);
    let without_path = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let without_port = without_path
        .rsplit_once(':')
        // `a.b.c:8443` and `[::1]:8443` carry a port; an IPv6 literal such as
        // `::1` or `fe80::1` does not, and splitting one would destroy it.
        .filter(|(host, port)| {
            !host.is_empty()
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && (host.contains('[') || !host.contains(':'))
        })
        .map_or(without_path, |(host, _port)| host);
    without_port
        .trim()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(&str, &str)]) -> HeaderMap {
        let mut out = HeaderMap::new();
        for (name, value) in items {
            out.insert(
                header_name(name).expect("name"),
                header_value(value).expect("value"),
            );
        }
        out
    }

    #[test]
    fn strips_hop_by_hop_and_connection_named() {
        let mut headers = map(&[
            ("connection", "x-secret, close"),
            ("x-secret", "value"),
            ("keep-alive", "timeout=5"),
            ("accept", "*/*"),
        ]);
        strip_hop_by_hop(&mut headers);
        assert!(headers.get("connection").is_none());
        assert!(headers.get("x-secret").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert_eq!(
            headers.get("accept").and_then(|v| v.to_str().ok()),
            Some("*/*")
        );
    }

    #[test]
    fn passthrough_none_forwards_nothing() {
        let inbound = map(&[("x-a", "1"), ("content-type", "application/json")]);
        let rules = RequestHeaders::default();
        let out = build_request_headers(&inbound, &rules, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn allowlist_forwards_only_listed() {
        let inbound = map(&[("x-a", "1"), ("x-b", "2")]);
        let rules = RequestHeaders {
            passthrough: crate::domain::model::Passthrough::Allowlist,
            passthrough_allowlist: vec!["x-a".to_owned()],
            ..RequestHeaders::default()
        };
        let out = build_request_headers(&inbound, &rules, &[]);
        assert_eq!(out.get("x-a").and_then(|v| v.to_str().ok()), Some("1"));
        assert!(out.get("x-b").is_none());
    }

    #[test]
    fn set_add_remove_rules_apply() {
        let rules = RequestHeaders {
            set: std::iter::once(("x-set".to_owned(), "1".to_owned())).collect(),
            add: std::iter::once(("x-add".to_owned(), "2".to_owned())).collect(),
            remove: vec!["x-drop".to_owned()],
            passthrough: crate::domain::model::Passthrough::All,
            passthrough_allowlist: Vec::new(),
        };
        let inbound = map(&[("x-drop", "old")]);
        let mut out = build_request_headers(&inbound, &rules, &[]);
        assert_eq!(out.get("x-set").and_then(|v| v.to_str().ok()), Some("1"));
        assert_eq!(out.get("x-add").and_then(|v| v.to_str().ok()), Some("2"));
        assert!(out.get("x-drop").is_none());
        apply_response_rules(
            &mut out,
            &ResponseHeaders {
                remove: vec!["x-set".to_owned()],
                ..ResponseHeaders::default()
            },
        );
        assert!(out.get("x-set").is_none());
    }

    #[test]
    fn bare_host_strips_scheme_port_and_path() {
        assert_eq!(bare_host("us.vendor.com"), "us.vendor.com");
        assert_eq!(bare_host("https://us.vendor.com:8443/a/b"), "us.vendor.com");
        assert_eq!(bare_host("  US.Vendor.com. "), "us.vendor.com");
        assert_eq!(bare_host("[::1]:8080"), "::1");
    }

    #[test]
    fn target_host_header_constant_is_lowercase() {
        assert_eq!(TARGET_HOST_HEADER, "x-oagw-target-host");
        assert_eq!(ERROR_SOURCE_HEADER, "x-oagw-error-source");
    }
}
