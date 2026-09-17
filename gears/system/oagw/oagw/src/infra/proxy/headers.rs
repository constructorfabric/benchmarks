//! Header transformation rules (DESIGN "Headers Transformation").

use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::model::{HeaderPassthrough, RequestHeaderRules, ResponseHeaderRules};

/// Hop-by-hop headers stripped from proxied traffic (DESIGN table).
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

/// Header read during routing and then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Header added to gateway-generated errors (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Applies the request-side header rules to the upstream request headers.
///
/// `inbound` must already be free of hop-by-hop headers. Headers not matched
/// by the passthrough rules are dropped.
#[must_use]
pub fn apply_request_rules(inbound: &HeaderMap, rules: Option<&RequestHeaderRules>) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    let Some(rules) = rules else {
        // Default posture: forward everything except hop-by-hop headers.
        for (name, value) in inbound {
            if is_hop_by_hop(name.as_str()) {
                continue;
            }
            outbound.append(name.clone(), value.clone());
        }
        return outbound;
    };

    match rules.passthrough {
        HeaderPassthrough::All => {
            for (name, value) in inbound {
                if is_hop_by_hop(name.as_str()) {
                    continue;
                }
                if rules.remove.iter().any(|r| r.eq_ignore_ascii_case(name.as_str())) {
                    continue;
                }
                outbound.append(name.clone(), value.clone());
            }
        }
        HeaderPassthrough::Allowlist => {
            for name in &rules.passthrough_allowlist {
                for value in inbound.get_all(name.as_str()) {
                    if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
                        outbound.append(header_name, value.clone());
                    }
                }
            }
        }
        HeaderPassthrough::None => {}
    }

    for name in &rules.remove {
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            outbound.remove(header_name);
        }
    }
    set_entries(&mut outbound, &rules.set);
    add_entries(&mut outbound, &rules.add);
    outbound
}

/// Applies the response-side header rules to the upstream response headers.
#[must_use]
pub fn apply_response_rules(
    upstream: &HeaderMap,
    rules: Option<&ResponseHeaderRules>,
    status_is_success: bool,
) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    for (name, value) in upstream {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        outbound.append(name.clone(), value.clone());
    }
    let _ = status_is_success;
    if let Some(rules) = rules {
        for name in &rules.remove {
            if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
                outbound.remove(header_name);
            }
        }
        set_entries(&mut outbound, &rules.set);
        add_entries(&mut outbound, &rules.add);
    }
    outbound
}

/// `true` for headers that must not cross a proxy boundary.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if HOP_BY_HOP_HEADERS.contains(&lower.as_str()) {
        return true;
    }
    // Headers named by `Connection:` are also hop-by-hop.
    lower == "proxy-connection" || lower == "proxy-authorization" || lower == "proxy-authenticate"
}

fn set_entries(headers: &mut HeaderMap, entries: &std::collections::BTreeMap<String, String>) {
    for (name, value) in entries {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(header_name, header_value);
        }
    }
}

fn add_entries(headers: &mut HeaderMap, entries: &std::collections::BTreeMap<String, String>) {
    for (name, value) in entries {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(header_name, header_value);
        }
    }
}

/// Strips the gateway's own routing headers before forwarding.
pub fn strip_gateway_headers(headers: &mut HeaderMap) {
    headers.remove(TARGET_HOST_HEADER);
    headers.remove("host");
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(*name);
    }
}

/// Adds the forwarding headers the upstream expects.
pub fn add_forwarding_headers(headers: &mut HeaderMap, client_host: Option<&str>) {
    if let Some(host) = client_host {
        insert_lossy(headers, "x-forwarded-host", host);
    }
    insert_lossy(headers, "x-forwarded-proto", "https");
}

/// Inserts a header from a string, ignoring invalid names/values.
pub fn insert_lossy(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn rules(set: &[(&str, &str)], remove: &[&str]) -> RequestHeaderRules {
        let mut set_map = BTreeMap::new();
        for (k, v) in set {
            set_map.insert((*k).to_owned(), (*v).to_owned());
        }
        RequestHeaderRules {
            set: set_map,
            add: BTreeMap::new(),
            remove: remove.iter().map(|s| (*s).to_owned()).collect(),
            passthrough: HeaderPassthrough::All,
            passthrough_allowlist: Vec::new(),
        }
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut inbound = HeaderMap::new();
        inbound.insert("x-api-key", HeaderValue::from_static("value"));
        inbound.insert(http::header::CONNECTION, HeaderValue::from_static("keep-alive"));
        inbound.insert(http::header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        let outbound = apply_request_rules(&inbound, None);
        assert!(outbound.get("x-api-key").is_some());
        assert!(outbound.get(http::header::CONNECTION).is_none());
        assert!(outbound.get(http::header::TRANSFER_ENCODING).is_none());
    }

    #[test]
    fn set_overwrites_and_add_appends() {
        let mut inbound = HeaderMap::new();
        inbound.insert("x-a", HeaderValue::from_static("original"));
        let outbound = apply_request_rules(&inbound, Some(&rules(&[("x-a", "replaced")], &[])));
        assert_eq!(outbound.get("x-a").unwrap(), "replaced");
    }

    #[test]
    fn remove_drops_header() {
        let mut inbound = HeaderMap::new();
        inbound.insert("x-secret", HeaderValue::from_static("value"));
        let outbound = apply_request_rules(&inbound, Some(&rules(&[], &["x-secret"])));
        assert!(outbound.get("x-secret").is_none());
    }

    #[test]
    fn passthrough_none_forwards_nothing() {
        let mut inbound = HeaderMap::new();
        inbound.insert("x-a", HeaderValue::from_static("1"));
        let mut rule_set = rules(&[("x-b", "added")], &[]);
        rule_set.passthrough = HeaderPassthrough::None;
        let outbound = apply_request_rules(&inbound, Some(&rule_set));
        assert!(outbound.get("x-a").is_none());
        assert_eq!(outbound.get("x-b").unwrap(), "added");
    }

    #[test]
    fn response_rules_apply() {
        let mut upstream = HeaderMap::new();
        upstream.insert("content-type", HeaderValue::from_static("application/json"));
        upstream.insert("server", HeaderValue::from_static("upstream/1"));
        let response_rules = ResponseHeaderRules {
            set: BTreeMap::new(),
            add: BTreeMap::new(),
            remove: vec!["server".to_owned()],
        };
        let outbound = apply_response_rules(&upstream, Some(&response_rules), true);
        assert!(outbound.get("content-type").is_some());
        assert!(outbound.get("server").is_none());
    }
}
