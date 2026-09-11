//! Header transformation: hop-by-hop and routing-header stripping,
//! passthrough-mode filtering, the `remove`/`set`/`add` header plan, `Host`
//! rewriting, and CORS response-header application
//! (`cpt-cf-oagw-algo-header-transformation`, `cpt-cf-oagw-dod-hop-by-hop-stripping`,
//! `cpt-cf-oagw-dod-header-plan-application`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use axum::http::header::{
    self, ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, CONTENT_TYPE, VARY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::model::{CorsConfig, Passthrough};
use crate::domain::proxy::endpoint::TARGET_HOST_HEADER;
use crate::domain::resolve::{RequestHeaderPlan, ResponseHeaderPlan};
use crate::error::OagwError;

/// The eight hop-by-hop header names stripped from every outbound request
/// (`cpt-cf-oagw-dod-hop-by-hop-stripping`). `pub(crate)` so the streaming
/// feature's WebSocket handshake header builder
/// (`crate::domain::proxy::websocket`) can reuse the exact same set while
/// exempting `Connection`/`Upgrade` from it
/// (`cpt-cf-oagw-dod-ws-upgrade-headers-survive`).
pub(crate) fn hop_by_hop_names() -> [HeaderName; 8] {
    [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ]
}

/// `true` for a header that is never forwarded regardless of the
/// passthrough mode: `Host` and `X-OAGW-Target-Host` (consumed for routing),
/// plus every hop-by-hop header.
fn is_stripped_unconditionally(name: &HeaderName) -> bool {
    *name == header::HOST || *name == TARGET_HOST_HEADER || hop_by_hop_names().contains(name)
}

/// `true` when `name` survives the plan's passthrough mode
/// (`cpt-cf-oagw-algo-header-transformation`): `Content-Type` is always
/// forwarded regardless of mode, since it describes the outbound body
/// itself rather than being a discretionary passthrough header. `pub(crate)`
/// so the WebSocket handshake header builder can apply the same passthrough
/// rule to non-handshake headers (`cpt-cf-oagw-dod-ws-upgrade-headers-survive`).
pub(crate) fn forwarded_by_passthrough(name: &HeaderName, plan: &RequestHeaderPlan) -> bool {
    if *name == CONTENT_TYPE {
        return true;
    }
    match plan.passthrough {
        Passthrough::None => false,
        Passthrough::All => true,
        Passthrough::Allowlist => plan.passthrough_allowlist.iter().any(|allowed| {
            HeaderName::from_bytes(allowed.as_bytes())
                .is_ok_and(|allowed_name| allowed_name == *name)
        }),
    }
}

/// Builds the outbound request header set: strips routing and hop-by-hop
/// headers, applies the passthrough mode, then the plan's `remove`, `set`,
/// and `add` operations in that order, then rewrites `Host` to
/// `target_host` (`cpt-cf-oagw-dod-header-plan-application`,
/// `cpt-cf-oagw-dod-hop-by-hop-stripping`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a `set`/`add` header name or
/// value is malformed (including one carrying a carriage return or line
/// feed, which [`HeaderValue::from_str`] itself refuses).
// @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-build-fn-01
pub fn build_outbound_headers(
    inbound: &HeaderMap,
    plan: &RequestHeaderPlan,
    target_host: &str,
) -> Result<HeaderMap, OagwError> {
    let mut outbound = copy_forwarded_headers(inbound, plan);
    apply_remove(&mut outbound, &plan.remove);
    apply_set(&mut outbound, &plan.set)?;
    apply_add(&mut outbound, &plan.add)?;
    let host_value = HeaderValue::from_str(target_host).map_err(|_| {
        OagwError::validation_error("selected upstream host is not a valid header value")
    })?;
    outbound.insert(header::HOST, host_value);
    Ok(outbound)
}

fn copy_forwarded_headers(inbound: &HeaderMap, plan: &RequestHeaderPlan) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    for (name, value) in inbound {
        if is_stripped_unconditionally(name) {
            continue;
        }
        if forwarded_by_passthrough(name, plan) {
            outbound.append(name.clone(), value.clone());
        }
    }
    outbound
}

/// `pub(crate)` so the WebSocket handshake header builder applies the same
/// plan `remove` step to the outbound handshake headers
/// (`cpt-cf-oagw-algo-upgrade-header-preservation`).
pub(crate) fn apply_remove(outbound: &mut HeaderMap, remove: &[String]) {
    for name in remove {
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            outbound.remove(header_name);
        }
    }
}

/// `pub(crate)` so the WebSocket handshake header builder applies the same
/// plan `set` step to the outbound handshake headers
/// (`cpt-cf-oagw-algo-upgrade-header-preservation`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a header name or value is
/// malformed.
pub(crate) fn apply_set(
    outbound: &mut HeaderMap,
    set: &std::collections::BTreeMap<String, String>,
) -> Result<(), OagwError> {
    for (name, value) in set {
        let (header_name, header_value) = parse_header_pair(name, value)?;
        outbound.insert(header_name, header_value);
    }
    Ok(())
}

/// `pub(crate)` so the WebSocket handshake header builder applies the same
/// plan `add` step to the outbound handshake headers
/// (`cpt-cf-oagw-algo-upgrade-header-preservation`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a header name or value is
/// malformed.
pub(crate) fn apply_add(
    outbound: &mut HeaderMap,
    add: &std::collections::BTreeMap<String, String>,
) -> Result<(), OagwError> {
    for (name, value) in add {
        let (header_name, header_value) = parse_header_pair(name, value)?;
        outbound.append(header_name, header_value);
    }
    Ok(())
}

fn parse_header_pair(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), OagwError> {
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| OagwError::validation_error(format!("'{name}' is not a valid header name")))?;
    let header_value = HeaderValue::from_str(value).map_err(|_| {
        OagwError::validation_error(format!(
            "value for header '{name}' is not a valid header value"
        ))
    })?;
    Ok((header_name, header_value))
}
// @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-build-fn-01

/// Applies the response header plan's `remove`, `set`, and `add` operations
/// to the outbound (client-facing) header set
/// (`cpt-cf-oagw-dod-header-plan-application`).
// @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-response-fn-01
pub fn apply_response_header_plan(headers: &mut HeaderMap, plan: &ResponseHeaderPlan) {
    apply_remove(headers, &plan.remove);
    // By the time a response header plan runs, the upstream response is
    // already committed (including on the streaming path, where the body may
    // already be flowing to the caller): there is no error response left to
    // return, so a malformed `set`/`add` entry is deliberately skipped rather
    // than propagated, leaving every other header operation in the plan
    // still applied. `drop` makes that discard explicit rather than silent.
    drop(apply_set(headers, &plan.set));
    drop(apply_add(headers, &plan.add));
}
// @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-response-fn-01

/// Strips the same eight hop-by-hop header names [`hop_by_hop_names`] lists
/// from the upstream RESPONSE headers, before they become the client-facing
/// set (`cpt-cf-oagw-dod-hop-by-hop-stripping`, CODE2-F-002): the request
/// direction already strips them via `copy_forwarded_headers`, but nothing
/// stripped them from the response on either the buffered or the streaming
/// path, so a copied-through `Transfer-Encoding: chunked` or
/// `Connection: keep-alive` could disagree with the bytes this gateway's own
/// server actually puts on the wire — the header/body-length ambiguity that
/// enables response smuggling in a downstream cache or proxy. Called before
/// the response header plan runs, symmetric with the request direction's
/// unconditional-strip-then-plan order, so an operator-configured `set`/`add`
/// can still reintroduce one of these names deliberately.
pub fn strip_hop_by_hop_response_headers(headers: &mut HeaderMap) {
    for name in hop_by_hop_names() {
        headers.remove(name);
    }
}

/// Adds the CORS response headers to a permitted actual-request response:
/// `Access-Control-Allow-Origin` (echoing the request origin),
/// `Access-Control-Expose-Headers` when configured,
/// `Access-Control-Allow-Credentials` when enabled, and `Vary: Origin`
/// (`cpt-cf-oagw-dod-cors-request-enforcement`).
// @cpt-begin:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-cors-response-headers-fn-01
pub fn apply_cors_response_headers(
    headers: &mut HeaderMap,
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
) {
    let (Some(cors), Some(origin)) = (cors, origin) else {
        return;
    };
    if !cors.enabled {
        return;
    }
    let Ok(origin_value) = HeaderValue::from_str(origin) else {
        return;
    };
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    if cors.allow_credentials {
        headers.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    append_vary(headers, "Origin");
}
// @cpt-end:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-cors-response-headers-fn-01

fn append_vary(headers: &mut HeaderMap, value: &str) {
    let combined = match headers
        .get(VARY)
        .and_then(|existing| existing.to_str().ok())
    {
        Some(existing) if !existing.is_empty() => format!("{existing}, {value}"),
        _ => value.to_owned(),
    };
    if let Ok(header_value) = HeaderValue::from_str(&combined) {
        headers.insert(VARY, header_value);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_cors_response_headers, apply_response_header_plan, build_outbound_headers,
        strip_hop_by_hop_response_headers,
    };
    use crate::domain::model::{CorsConfig, Sharing};
    use crate::domain::resolve::{RequestHeaderPlan, ResponseHeaderPlan};
    use axum::http::{HeaderMap, HeaderValue};
    use std::collections::BTreeMap;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("valid name"),
                HeaderValue::from_str(value).expect("valid value"),
            );
        }
        headers
    }

    // @cpt-begin:cpt-cf-oagw-dod-hop-by-hop-stripping:p1:inst-hop-by-hop-test-01
    #[test]
    fn hop_by_hop_and_routing_headers_are_never_forwarded() {
        let inbound = header_map(&[
            ("connection", "keep-alive"),
            ("te", "trailers"),
            ("trailer", "X-T"),
            ("upgrade", "h2c"),
            ("keep-alive", "timeout=5"),
            ("proxy-authenticate", "Basic"),
            ("proxy-authorization", "Basic abc"),
            ("transfer-encoding", "chunked"),
            ("x-oagw-target-host", "us.vendor.com"),
            ("x-keep", "1"),
        ]);
        let plan = RequestHeaderPlan {
            passthrough: crate::domain::model::Passthrough::All,
            ..RequestHeaderPlan::default()
        };

        let outbound =
            build_outbound_headers(&inbound, &plan, "origin.example.com").expect("must build");

        for name in [
            "connection",
            "te",
            "trailer",
            "upgrade",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "transfer-encoding",
            "x-oagw-target-host",
        ] {
            assert!(!outbound.contains_key(name), "'{name}' must be stripped");
        }
        assert!(outbound.contains_key("x-keep"));
    }
    // @cpt-end:cpt-cf-oagw-dod-hop-by-hop-stripping:p1:inst-hop-by-hop-test-01

    // @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-host-rewrite-test-01
    #[test]
    fn host_is_always_rewritten_to_the_selected_endpoint() {
        let inbound = header_map(&[("host", "gateway.internal")]);
        let plan = RequestHeaderPlan::default();
        let outbound =
            build_outbound_headers(&inbound, &plan, "origin.example.com").expect("must build");
        assert_eq!(
            outbound.get("host").and_then(|v| v.to_str().ok()),
            Some("origin.example.com")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-host-rewrite-test-01

    // @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-remove-set-add-test-01
    #[test]
    fn remove_set_and_add_apply_in_order() {
        let inbound = header_map(&[("x-drop", "1")]);
        let plan = RequestHeaderPlan {
            set: BTreeMap::from([("X-Set".to_owned(), "a".to_owned())]),
            add: BTreeMap::from([("X-Add".to_owned(), "b".to_owned())]),
            remove: vec!["X-Drop".to_owned()],
            passthrough: crate::domain::model::Passthrough::All,
            ..RequestHeaderPlan::default()
        };

        let outbound =
            build_outbound_headers(&inbound, &plan, "origin.example.com").expect("must build");

        assert!(!outbound.contains_key("x-drop"));
        assert_eq!(
            outbound.get("x-set").and_then(|v| v.to_str().ok()),
            Some("a")
        );
        assert_eq!(
            outbound.get("x-add").and_then(|v| v.to_str().ok()),
            Some("b")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-remove-set-add-test-01

    #[test]
    fn passthrough_none_forwards_nothing_but_content_type() {
        let inbound = header_map(&[("x-custom", "1"), ("content-type", "application/json")]);
        let plan = RequestHeaderPlan::default();
        let outbound =
            build_outbound_headers(&inbound, &plan, "origin.example.com").expect("must build");
        assert!(!outbound.contains_key("x-custom"));
        assert!(outbound.contains_key("content-type"));
    }

    #[test]
    fn passthrough_allowlist_forwards_only_listed_names() {
        let inbound = header_map(&[("x-allowed", "1"), ("x-blocked", "2")]);
        let plan = RequestHeaderPlan {
            passthrough: crate::domain::model::Passthrough::Allowlist,
            passthrough_allowlist: vec!["x-allowed".to_owned()],
            ..RequestHeaderPlan::default()
        };
        let outbound =
            build_outbound_headers(&inbound, &plan, "origin.example.com").expect("must build");
        assert!(outbound.contains_key("x-allowed"));
        assert!(!outbound.contains_key("x-blocked"));
    }

    // @cpt-begin:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-response-plan-test-01
    #[test]
    fn response_plan_removes_and_sets_headers() {
        let mut headers = header_map(&[("server", "nginx")]);
        let plan = ResponseHeaderPlan {
            set: BTreeMap::from([("X-Gw".to_owned(), "1".to_owned())]),
            remove: vec!["Server".to_owned()],
            ..ResponseHeaderPlan::default()
        };

        apply_response_header_plan(&mut headers, &plan);

        assert!(!headers.contains_key("server"));
        assert_eq!(headers.get("x-gw").and_then(|v| v.to_str().ok()), Some("1"));
    }
    // @cpt-end:cpt-cf-oagw-dod-header-plan-application:p1:inst-header-response-plan-test-01

    #[test]
    fn cors_response_headers_are_added_for_an_enabled_permitted_origin() {
        let mut headers = HeaderMap::new();
        let cors = CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec![],
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: true,
        };
        apply_cors_response_headers(&mut headers, Some(&cors), Some("https://app.example.com"));

        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            headers
                .get("access-control-expose-headers")
                .and_then(|v| v.to_str().ok()),
            Some("X-Request-ID")
        );
        assert_eq!(
            headers
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            headers.get("vary").and_then(|v| v.to_str().ok()),
            Some("Origin")
        );
    }

    // CODE2-F-002 regression: hop-by-hop headers must be stripped from the
    // upstream RESPONSE headers too, symmetric with the request direction.
    #[test]
    fn hop_by_hop_response_headers_are_stripped() {
        let mut headers = header_map(&[
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("x-keep", "1"),
        ]);
        strip_hop_by_hop_response_headers(&mut headers);
        assert!(!headers.contains_key("transfer-encoding"));
        assert!(!headers.contains_key("connection"));
        assert!(headers.contains_key("x-keep"));
    }

    #[test]
    fn disabled_cors_adds_no_headers() {
        let mut headers = HeaderMap::new();
        let cors = CorsConfig {
            sharing: Sharing::Private,
            enabled: false,
            allowed_origins: vec![],
            allowed_methods: vec![],
            expose_headers: vec![],
            allow_credentials: false,
        };
        apply_cors_response_headers(&mut headers, Some(&cors), Some("https://app.example.com"));
        assert!(headers.is_empty());
    }
}
