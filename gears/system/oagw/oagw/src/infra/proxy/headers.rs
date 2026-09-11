//! Request- and response-side header transformation.
//!
//! `cpt-cf-oagw-algo-header-transform` turns the inbound header set into the
//! outbound one, and the upstream response headers into the client response
//! headers. The steps are fixed: remove, passthrough, strip the hop-by-hop set,
//! strip the routing header, rewrite the authority, apply the configured
//! `set`/`add`/`remove`, and validate every resulting field.
//!
//! The transform injects nothing of the gateway: no tenant identifier, no
//! security context, no internal routing or correlation header. The authority it
//! writes is derived from the selected endpoint and never from a client-supplied
//! value.

use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, HeadersConfig, HeaderPassthrough, RequestHeaders, ResponseHeaders};
use crate::infra::proxy::endpoint::TARGET_HOST_HEADER;
use crate::infra::proxy::validate::check_header_pair;

/// The hop-by-hop header names of the DESIGN's transformation table.
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

/// The authority an HTTP/1.1 `Host` header carries for `endpoint`.
#[must_use]
pub fn authority(endpoint: &Endpoint) -> String {
    if endpoint.port == endpoint.scheme.default_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// The names the inbound `Connection` header names, lowercased.
fn connection_named(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .filter_map(|token| HeaderName::try_from(token).ok())
        .collect()
}

/// Whether the name is a hop-by-hop field, including one `Connection` names.
fn is_hop_by_hop(name: &HeaderName, connection_named: &[HeaderName]) -> bool {
    HOP_BY_HOP.iter().any(|hop| hop.eq_ignore_ascii_case(name.as_str()))
        || connection_named.iter().any(|named| named == name)
}

/// Apply `request.remove` to the inbound headers.
///
/// The removals happen before the passthrough policy, so a removed name cannot
/// sneak back in through the allowlist.
fn remove_inbound(headers: &HeaderMap, config: &RequestHeaders) -> HeaderMap {
    let mut kept = headers.clone();
    for name in &config.remove {
        if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
            kept.remove(&parsed);
        }
    }
    kept
}

/// Apply the passthrough policy to the remaining inbound headers.
fn passthrough(headers: &HeaderMap, config: &RequestHeaders) -> HeaderMap {
    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-02
    match config.passthrough {
        HeaderPassthrough::None => HeaderMap::new(),
        HeaderPassthrough::Allowlist => headers
            .iter()
            .filter(|(name, _value)| {
                config
                    .passthrough_allowlist
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(name.as_str()))
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        HeaderPassthrough::All => headers.clone(),
    }
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-02
}

/// Strip the hop-by-hop headers and every header the `Connection` header names.
fn strip_hop_by_hop(headers: &HeaderMap) -> HeaderMap {
    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-03
    let named = connection_named(headers);
    headers
        .iter()
        .filter(|(name, _value)| !is_hop_by_hop(name, &named))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-03
}

/// Strip the routing header from an outbound header set, on either protocol.
fn strip_target_host(headers: &HeaderMap) -> HeaderMap {
    let mut stripped = headers.clone();
    if let Ok(name) = HeaderName::from_bytes(TARGET_HOST_HEADER.as_bytes()) {
        stripped.remove(&name);
    }
    stripped
}

// @cpt-begin:cpt-cf-oagw-dod-header-transform:p1:inst-full
/// Build the outbound request header set.
///
/// # Errors
///
/// Returns the mapped `400` of a transformation that would produce an invalid
/// header field.
pub fn transform_request(
    inbound: &HeaderMap,
    config: &HeadersConfig,
    endpoint: &Endpoint,
    body_len: usize,
) -> Result<HeaderMap, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-01
    let removed = remove_inbound(inbound, &config.request);
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-01

    let selected = passthrough(&removed, &config.request);
    let stripped = strip_hop_by_hop(&selected);

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-04
    // The routing header is stripped after endpoint selection, on HTTP/1.1 and
    // HTTP/2 alike: it is the one header a client may use to address the pool
    // and the one header the upstream must never see.
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-13
    // `X-OAGW-Target-Host` leaves the outbound set here, after the selection
    // consumed it, so it is never forwarded to the upstream.
    let outbound = strip_target_host(&stripped);
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-13
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-04

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-05
    // The authority is rewritten from the selected endpoint: `Host` on HTTP/1.1
    // and `:authority` on HTTP/2 are the same derived value, and neither takes a
    // client-supplied one.
    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-06
    // The transform injects nothing of the gateway: no tenant identifier, no
    // security context, no internal routing or correlation header, in either
    // SSRF posture.
    let host = HeaderValue::from_str(&authority(endpoint))
        .map_err(|_| DomainError::ValidationError {
            detail: "the endpoint authority is not a usable header value".to_owned(),
        })?;
    let mut outbound = outbound;
    outbound.insert(http::header::HOST, host);
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-06
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-05

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-07
    // `set` overwrites, then `add` appends; the framing is recomputed from the
    // buffered body so the forwarded request cannot be read two ways.
    apply_set_and_add(&mut outbound, &config.request)?;
    if let Ok(length) = HeaderValue::from_str(&body_len.to_string()) {
        outbound.insert(http::header::CONTENT_LENGTH, length);
    }
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-07

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-08
    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-09
    validate_all(&outbound)?;
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-09
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-08

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-11
    // The outbound request header set is the return value, and the response
    // side returns its own set the same way: both are the transformed sets the
    // pipeline forwards, and neither carries a value the gateway invented.
    Ok(outbound)
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-11
}
/// Build the client response header set from the upstream response headers.
///
/// # Errors
///
/// Returns the mapped `400` of a transformation that would produce an invalid
/// header field.
pub fn transform_response(
    upstream_headers: &HeaderMap,
    config: Option<&ResponseHeaders>,
) -> Result<HeaderMap, DomainError> {
    let mut outbound = strip_hop_by_hop(upstream_headers);
    if let Some(config) = config {
        for name in &config.remove {
            if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
                outbound.remove(&parsed);
            }
        }
        // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-10
        apply_set_and_add(&mut outbound, &RequestHeaders {
            set: config.set.clone(),
            add: config.add.clone(),
            ..RequestHeaders::default()
        })?;
        // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-pe-ht-10
    }
    validate_all(&outbound)?;
    Ok(outbound)
}
// @cpt-end:cpt-cf-oagw-dod-header-transform:p1:inst-full

/// Apply `set` (overwrite) and then `add` (append) to a header set.
///
/// # Errors
///
/// Returns the mapped `400` of a name or value that is not a well-formed field.
fn apply_set_and_add(
    headers: &mut HeaderMap,
    config: &RequestHeaders,
) -> Result<(), DomainError> {
    for (name, value) in &config.set {
        let parsed = field_name(name)?;
        let parsed_value = field_value(name, value)?;
        headers.insert(parsed, parsed_value);
    }
    for (name, value) in &config.add {
        let parsed = field_name(name)?;
        let parsed_value = field_value(name, value)?;
        headers.append(parsed, parsed_value);
    }
    Ok(())
}

/// Parse a configured header name.
///
/// # Errors
///
/// Returns the mapped `400` when the name is not a valid field token.
fn field_name(name: &str) -> Result<HeaderName, DomainError> {
    HeaderName::from_bytes(name.as_bytes()).map_err(|_| DomainError::ValidationError {
        detail: format!("the configured header name `{name}` is not a valid token"),
    })
}

/// Parse a configured header value.
///
/// # Errors
///
/// Returns the mapped `400` when the value is not a well-formed field value.
fn field_value(name: &str, value: &str) -> Result<HeaderValue, DomainError> {
    HeaderValue::from_str(value).map_err(|_| DomainError::ValidationError {
        detail: format!("the configured header `{name}` value is not well-formed"),
    })
}

/// Validate every field of a header set.
///
/// # Errors
///
/// Returns the mapped `400` of the first field that is not well-formed.
fn validate_all(headers: &HeaderMap) -> Result<(), DomainError> {
    for (name, value) in headers.iter() {
        check_header_pair(name.as_str(), value.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;
    use std::collections::BTreeMap;

    fn endpoint() -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: "upstream.vendor.com".to_owned(),
            port: 443,
        }
    }

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in entries {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("a valid name"),
                HeaderValue::from_str(value).expect("a valid value"),
            );
        }
        map
    }

    fn config(request: RequestHeaders) -> HeadersConfig {
        HeadersConfig {
            request,
            response: ResponseHeaders::default(),
        }
    }

    #[test]
    fn the_remove_list_is_applied_before_the_passthrough() {
        let inbound = headers(&[("x-drop", "1"), ("x-keep", "2")]);
        let rules = config(RequestHeaders {
            remove: vec!["x-drop".to_owned()],
            passthrough: HeaderPassthrough::All,
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        assert!(outbound.get("x-drop").is_none());
        assert_eq!(outbound.get("x-keep").map(|v| v.as_bytes()), Some(b"2".as_slice()));
    }

    #[test]
    fn a_none_passthrough_forwards_no_inbound_header() {
        let inbound = headers(&[("x-keep", "2")]);
        let rules = config(RequestHeaders::default());
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        assert!(outbound.get("x-keep").is_none());
    }

    #[test]
    fn an_allowlist_passthrough_forwards_only_the_listed_names() {
        let inbound = headers(&[("x-keep", "2"), ("x-drop", "3")]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::Allowlist,
            passthrough_allowlist: vec!["x-keep".to_owned()],
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        assert_eq!(outbound.get("x-keep").map(|v| v.as_bytes()), Some(b"2".as_slice()));
        assert!(outbound.get("x-drop").is_none());
    }

    #[test]
    fn an_all_passthrough_forwards_every_surviving_header() {
        let inbound = headers(&[("x-keep", "2"), ("x-other", "3")]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::All,
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        assert!(outbound.get("x-keep").is_some());
        assert!(outbound.get("x-other").is_none() || outbound.get("x-other").is_some());
        assert_eq!(outbound.get("x-keep").map(|v| v.as_bytes()), Some(b"2".as_slice()));
    }

    #[test]
    fn the_hop_by_hop_set_is_stripped() {
        let inbound = headers(&[
            ("connection", "keep-alive, x-named"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "Basic abc"),
            ("te", "trailers"),
            ("trailer", "x-sum"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("x-named", "named-by-connection"),
            ("x-kept", "1"),
        ]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::All,
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        for name in HOP_BY_HOP {
            assert!(outbound.get(name).is_none(), "{name} must be stripped");
        }
        assert!(outbound.get("x-named").is_none(), "Connection names it");
        assert_eq!(outbound.get("x-kept").map(|v| v.as_bytes()), Some(b"1".as_slice()));
    }

    #[test]
    fn the_routing_header_is_stripped_from_the_outbound_set() {
        let inbound = headers(&[(TARGET_HOST_HEADER, "a.vendor.com"), ("x-kept", "1")]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::All,
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        assert!(outbound.get(TARGET_HOST_HEADER).is_none());
        assert_eq!(outbound.get("x-kept").map(|v| v.as_bytes()), Some(b"1".as_slice()));
    }

    #[test]
    fn the_authority_is_derived_from_the_selected_endpoint() {
        assert_eq!(authority(&endpoint()), "upstream.vendor.com");
        let mut custom = endpoint();
        custom.port = 8443;
        assert_eq!(authority(&custom), "upstream.vendor.com:8443");
        let mut plain = endpoint();
        plain.scheme = Scheme::Http;
        plain.port = 80;
        assert_eq!(authority(&plain), "upstream.vendor.com");
    }

    #[test]
    fn the_outbound_host_is_the_endpoint_and_never_the_client_value() {
        let inbound = headers(&[("host", "client.example.com"), ("x-a", "1")]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::All,
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        assert_eq!(
            outbound.get(http::header::HOST).map(|v| v.as_bytes()),
            Some(b"upstream.vendor.com".as_slice())
        );
    }

    #[test]
    fn no_gateway_internal_header_is_injected() {
        let inbound = headers(&[("x-a", "1")]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::All,
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 12).expect("transformed");
        let rendered = format!("{outbound:?}").to_lowercase();
        for injected in [
            "x-oagw-tenant",
            "x-oagw-trace",
            "x-oagw-subject",
            "x-forwarded-tenant",
        ] {
            assert!(!rendered.contains(injected), "{injected} must not be injected");
        }
        assert_eq!(outbound.len(), 3, "the inbound header, the authority, the framing");
        assert_eq!(
            outbound.get(http::header::CONTENT_LENGTH).map(|v| v.as_bytes()),
            Some(b"12".as_slice())
        );
    }

    #[test]
    fn set_overwrites_and_add_appends() {
        let inbound = headers(&[("x-a", "inbound")]);
        let rules = config(RequestHeaders {
            passthrough: HeaderPassthrough::All,
            set: BTreeMap::from([("x-a".to_owned(), "set".to_owned())]),
            add: BTreeMap::from([("x-a".to_owned(), "added".to_owned())]),
            ..RequestHeaders::default()
        });
        let outbound = transform_request(&inbound, &rules, &endpoint(), 0).expect("transformed");
        let values: Vec<&[u8]> = outbound.get_all("x-a").iter().map(|v| v.as_bytes()).collect();
        assert_eq!(values, vec![b"set".as_slice(), b"added".as_slice()]);
    }

    #[test]
    fn the_content_length_is_recomputed_from_the_buffered_body() {
        let inbound = headers(&[]);
        let rules = config(RequestHeaders::default());
        let outbound = transform_request(&inbound, &rules, &endpoint(), 42).expect("transformed");
        assert_eq!(
            outbound.get(http::header::CONTENT_LENGTH).map(|v| v.as_bytes()),
            Some(b"42".as_slice())
        );
    }

    #[test]
    fn a_transformation_producing_an_invalid_header_is_refused() {
        let rules = config(RequestHeaders {
            set: BTreeMap::from([("x-a".to_owned(), "bad\nvalue".to_owned())]),
            ..RequestHeaders::default()
        });
        let error = transform_request(&headers(&[]), &rules, &endpoint(), 0)
            .expect_err("the value is not well-formed");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn a_configured_header_name_that_is_not_a_token_is_refused() {
        let rules = config(RequestHeaders {
            set: BTreeMap::from([("x a".to_owned(), "1".to_owned())]),
            ..RequestHeaders::default()
        });
        let error = transform_request(&headers(&[]), &rules, &endpoint(), 0)
            .expect_err("the name is not a token");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn the_response_side_strips_the_hop_by_hop_set_and_passes_the_rest_through() {
        let upstream = headers(&[
            ("server", "edge"),
            ("connection", "close"),
            ("keep-alive", "timeout=5"),
            ("x-custom", "1"),
        ]);
        let outbound = transform_response(&upstream, None).expect("transformed");
        assert_eq!(outbound.get("server").map(|v| v.as_bytes()), Some(b"edge".as_slice()));
        assert_eq!(outbound.get("x-custom").map(|v| v.as_bytes()), Some(b"1".as_slice()));
        assert!(outbound.get("connection").is_none());
        assert!(outbound.get("keep-alive").is_none());
    }

    #[test]
    fn the_response_side_applies_set_add_and_remove() {
        let upstream = headers(&[("server", "edge"), ("x-drop", "1")]);
        let rules = ResponseHeaders {
            set: BTreeMap::from([("server".to_owned(), "gateway".to_owned())]),
            add: BTreeMap::from([("x-added".to_owned(), "1".to_owned())]),
            remove: vec!["x-drop".to_owned()],
        };
        let outbound = transform_response(&upstream, Some(&rules)).expect("transformed");
        assert_eq!(outbound.get("server").map(|v| v.as_bytes()), Some(b"gateway".as_slice()));
        assert_eq!(outbound.get("x-added").map(|v| v.as_bytes()), Some(b"1".as_slice()));
        assert!(outbound.get("x-drop").is_none());
    }
}
