//! CORS handling: the pre-resolution preflight fast path, post-resolution
//! origin/method validation, and the post-relay response-header injection,
//! per `docs/features/cors-handling.md` (DECOMPOSITION entry 2.7,
//! `cpt-cf-oagw-feature-cors-handling`) and its normative source
//! `cpt-cf-oagw-adr-cors`.
//!
//! Every function below reads only the already-merged effective `cors`
//! object `crate::proxy::merge` hands it (via `EffectiveConfig::cors`,
//! itself sourced solely from Upstream-level `cors` objects across the
//! tenant chain -- `route.v1.schema.json` defines no top-level `cors`
//! property, so no Route-level `cors` object is ever read here); this
//! module performs no hierarchy walk of its own.

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use crate::model::upstream::{CorsConfig, CorsMethod};

/// Outcome of a CORS policy check: either continue forwarding unchanged, or
/// short-circuit the request with a rendered response (a preflight
/// response, or a `403` origin/method rejection).
pub(crate) enum CorsOutcome {
    Continue,
    ShortCircuit(Response),
}

const MAX_AGE_SECONDS: &str = "86400";

// ---------------------------------------------------------------------
// Preflight Fast Path (`cpt-cf-oagw-dod-cors-preflight-fast-path`)
// ---------------------------------------------------------------------

/// `cpt-cf-oagw-algo-cors-preflight-detect-and-respond`: `OPTIONS` +
/// `Origin` + `Access-Control-Request-Method` classifies the request as a
/// CORS preflight.
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-parse
fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-parse
    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-if-match
    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-classify
    method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-classify
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-if-match
}

/// Builds the permissive `204` preflight response: echoes the requested
/// origin/method/headers, adds a fixed `Access-Control-Max-Age`, the
/// documented `Vary` list, and the gateway error-source header (this `204`
/// is produced by OAGW itself, before any upstream is resolved, per
/// `cpt-cf-oagw-principle-error-source` applying to every response OAGW
/// returns).
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-if-classified
fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response_headers = HeaderMap::new();

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-allow-origin
    if let Some(origin) = headers.get(header::ORIGIN) {
        response_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-allow-origin

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-allow-methods
    if let Some(requested_method) = headers.get(header::ACCESS_CONTROL_REQUEST_METHOD) {
        response_headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            requested_method.clone(),
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-allow-methods

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-if-headers-present
    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-allow-headers
    if let Some(requested_headers) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        response_headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            requested_headers.clone(),
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-allow-headers
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-if-headers-present

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-max-age
    response_headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(MAX_AGE_SECONDS),
    );
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-max-age

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-vary
    response_headers.insert(
        header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    response_headers.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER_NAME),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-set-vary

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-return
    let mut response = StatusCode::NO_CONTENT.into_response();
    *response.headers_mut() = response_headers;
    response
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-return
}
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-if-classified

/// Pre-resolution request-classification hook
/// (`inst-proxy-fwd-preflight-hook`): the CORS preflight fast path runs
/// here, before any upstream resolution or tenant walk, and sits outside
/// the fixed post-resolution policy order entirely.
// @cpt-flow:cpt-cf-oagw-flow-cors-browser-preflight-request:p2
// @cpt-algo:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2
// @cpt-dod:cpt-cf-oagw-dod-cors-preflight-fast-path:p2
// @cpt-begin:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-request
// @cpt-begin:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-detect
pub(crate) fn preflight_fast_path(method: &Method, headers: &HeaderMap) -> CorsOutcome {
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-detect
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-request
    if is_preflight(method, headers) {
        // @cpt-begin:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-if-detected
        // @cpt-begin:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-return-204
        CorsOutcome::ShortCircuit(preflight_response(headers))
        // @cpt-end:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-return-204
        // @cpt-end:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-if-detected
    } else {
        // @cpt-begin:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-else
        // @cpt-begin:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-fallthrough
        // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-else
        // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-not-preflight
        // Not classified as a preflight: hand control back to the caller,
        // which continues into the normal proxy-core resolution path.
        CorsOutcome::Continue
        // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-not-preflight
        // @cpt-end:cpt-cf-oagw-algo-cors-preflight-detect-and-respond:p2:inst-cors-algo-preflight-else
        // @cpt-end:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-fallthrough
        // @cpt-end:cpt-cf-oagw-flow-cors-browser-preflight-request:p2:inst-cors-preflight-flow-else
    }
}

// ---------------------------------------------------------------------
// Origin Matching (`cpt-cf-oagw-algo-cors-match-origin`)
// ---------------------------------------------------------------------

/// `cpt-cf-oagw-algo-cors-match-origin`: exact, scheme- and port-sensitive
/// string matching, no regular-expression or substring matching; a
/// wildcard `allowed_origins` entry never matches when the effective
/// `allow_credentials` is `true`.
// @cpt-algo:cpt-cf-oagw-algo-cors-match-origin:p2
// @cpt-dod:cpt-cf-oagw-dod-cors-security-defaults:p2
// @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-parse
fn match_origin(origin: &str, cors: &CorsConfig) -> Option<String> {
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-parse
    // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-invalid-combo
    // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-drop-wildcard
    // A credentialed configuration never honors a wildcard origin entry,
    // whether that combination arose from a single resource or only after
    // hierarchical merge (`cpt-cf-oagw-dod-cors-security-defaults`).
    let drop_wildcard =
        cors.allow_credentials && cors.allowed_origins.iter().any(|entry| entry == "*");
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-drop-wildcard
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-invalid-combo

    let mut matched_exact = false;
    let mut matched_wildcard = false;
    // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-for-each
    for entry in &cors.allowed_origins {
        // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-wildcard-entry
        // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-wildcard-match
        if entry == "*" {
            if !drop_wildcard {
                matched_wildcard = true;
            }
        // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-wildcard-match
        // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-wildcard-entry
        // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-exact
        // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-exact-match
        } else if entry == origin {
            // Byte-for-byte identical: scheme, host and port are all
            // significant; no normalization is applied to either side.
            matched_exact = true;
        }
        // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-exact-match
        // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-exact
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-for-each

    // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-none-matched
    // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-return-fail
    // @cpt-begin:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-return-success
    if matched_exact {
        // Explicit entry: always echo the request's literal `Origin`.
        Some(origin.to_owned())
    } else if matched_wildcard {
        // Only reachable when `drop_wildcard` is `false` (`allow_credentials`
        // is `false`), so the wildcard literal is always safe to emit here.
        Some("*".to_owned())
    } else {
        None
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-return-success
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-return-fail
    // @cpt-end:cpt-cf-oagw-algo-cors-match-origin:p2:inst-cors-algo-origin-if-none-matched
}

// ---------------------------------------------------------------------
// Method Checking (`cpt-cf-oagw-algo-cors-check-method`)
// ---------------------------------------------------------------------

fn cors_method_str(method: CorsMethod) -> &'static str {
    match method {
        CorsMethod::Get => "GET",
        CorsMethod::Post => "POST",
        CorsMethod::Put => "PUT",
        CorsMethod::Patch => "PATCH",
        CorsMethod::Delete => "DELETE",
        CorsMethod::Head => "HEAD",
        CorsMethod::Options => "OPTIONS",
    }
}

/// `cpt-cf-oagw-algo-cors-check-method`: request method membership in the
/// effective `allowed_methods`.
// @cpt-algo:cpt-cf-oagw-algo-cors-check-method:p2
// @cpt-begin:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-parse
// @cpt-begin:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-if-present
// @cpt-begin:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-return-success
// @cpt-begin:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-else
// @cpt-begin:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-return-fail
fn method_allowed(method: &Method, cors: &CorsConfig) -> bool {
    cors.allowed_methods.iter().any(|allowed| {
        method
            .as_str()
            .eq_ignore_ascii_case(cors_method_str(*allowed))
    })
}
// @cpt-end:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-return-fail
// @cpt-end:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-else
// @cpt-end:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-return-success
// @cpt-end:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-if-present
// @cpt-end:cpt-cf-oagw-algo-cors-check-method:p2:inst-cors-algo-method-parse

// ---------------------------------------------------------------------
// Response rejection/header helpers
// ---------------------------------------------------------------------

/// `Vary: Origin` always accompanies an origin-dependent response, to
/// prevent cache poisoning; appended (not overwritten) so an existing
/// `Vary` value from elsewhere in the response is preserved.
fn append_vary_origin(headers: &mut HeaderMap) {
    let combined = match headers.get(header::VARY).and_then(|v| v.to_str().ok()) {
        Some(existing) if existing.split(',').any(|part| part.trim() == "Origin") => return,
        Some(existing) => format!("{existing}, Origin"),
        None => "Origin".to_owned(),
    };
    if let Ok(value) = HeaderValue::from_str(&combined) {
        headers.insert(header::VARY, value);
    }
}

/// Builds a `403` rejection carrying `X-OAGW-Error-Source: gateway` (via
/// the shared `OagwError`/`OagwErrorKind` catalog and renderer -- see
/// `docs/features/gear-foundation.md`'s `cpt-cf-oagw-dod-error-envelope`
/// "Accepted residual risk" note for why these two rows exist in that
/// catalog despite having no counterpart in `DESIGN.md`'s frozen table)
/// plus `Vary: Origin`, per `cpt-cf-oagw-dod-cors-actual-request-validation`
/// and `cpt-cf-oagw-dod-cors-response-headers`.
fn cors_rejection(kind: OagwErrorKind, detail: String) -> Response {
    let mut response = OagwError::new(kind, detail).into_response();
    append_vary_origin(response.headers_mut());
    response
}

// ---------------------------------------------------------------------
// Actual Cross-Origin Request Validation
// (`cpt-cf-oagw-dod-cors-actual-request-validation`)
// ---------------------------------------------------------------------

/// Post-resolution policy hook, first in the fixed order CORS -> rate-limit
/// -> plugin chain (`inst-proxy-fwd-policy-hook`): validates the request
/// `Origin` and method against the merged `cors` configuration, only when
/// the effective `cors.enabled` is `true` and the request carries an
/// `Origin` header.
// @cpt-flow:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2
// @cpt-dod:cpt-cf-oagw-dod-cors-actual-request-validation:p2
// @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-request
// @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-resolve
pub(crate) fn validate_origin_and_method(
    cors: Option<&CorsConfig>,
    headers: &HeaderMap,
    method: &Method,
) -> CorsOutcome {
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-resolve
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-request
    let Some(cors) = cors else {
        // No `cors` object resolves anywhere in the effective merge: the
        // schema default (`enabled: false`) applies.
        return CorsOutcome::Continue;
    };

    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-if-disabled
    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-skip-disabled
    // This is the same "perform no CORS processing" posture
    // `cpt-cf-oagw-dod-cors-security-defaults` documents (scope marker on
    // `match_origin`, above, to avoid a duplicate scope marker in this file).
    if !cors.enabled {
        // `cors.enabled: false`: perform no origin/method validation and
        // add no `Access-Control-*` response header; proxy-core forwards
        // exactly as it would a non-CORS request.
        return CorsOutcome::Continue;
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-skip-disabled
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-if-disabled

    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Not a cross-origin request: nothing to validate.
        return CorsOutcome::Continue;
    };

    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-else-enabled
    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-check-origin
    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-if-origin-fail
    if match_origin(origin, cors).is_none() {
        // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-return-403-origin
        let detail = format!("Origin '{origin}' not in allowed origins list");
        return CorsOutcome::ShortCircuit(cors_rejection(
            OagwErrorKind::CorsOriginNotAllowed,
            detail,
        ));
        // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-return-403-origin
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-if-origin-fail
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-check-origin

    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-else-origin-pass
    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-check-method
    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-if-method-fail
    if !method_allowed(method, cors) {
        // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-return-403-method
        let detail = format!("Method '{}' not in allowed methods list", method.as_str());
        return CorsOutcome::ShortCircuit(cors_rejection(
            OagwErrorKind::CorsMethodNotAllowed,
            detail,
        ));
        // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-return-403-method
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-if-method-fail
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-check-method

    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-else-method-pass
    // Origin and method both pass: continue toward forwarding. The caller
    // (`crate::proxy::engine`) performs the forward and this feature's
    // response-header injection separately.
    CorsOutcome::Continue
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-else-method-pass
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-else-origin-pass
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-else-enabled
}

// ---------------------------------------------------------------------
// Forwarded-Response Headers (`cpt-cf-oagw-dod-cors-response-headers`)
// ---------------------------------------------------------------------

/// Post-relay response-header hook (`inst-proxy-relay-headers`): adds
/// `Access-Control-*` and `Vary: Origin` to the relayed response, after the
/// configured `headers.response` rules have run and before the response is
/// committed to the caller.
// @cpt-dod:cpt-cf-oagw-dod-cors-response-headers:p2
// @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-forward
// @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-add-headers
pub(crate) fn inject_response_headers(
    cors: Option<&CorsConfig>,
    headers: &HeaderMap,
    response: &mut Response,
) {
    let Some(cors) = cors else {
        return;
    };
    if !cors.enabled {
        return;
    }

    // `Vary: Origin` is always present on a response from an upstream/route
    // whose effective `cors.enabled` is `true`, regardless of whether this
    // particular request happened to carry an `Origin` header.
    append_vary_origin(response.headers_mut());

    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return;
    };
    let Some(allow_origin) = match_origin(origin, cors) else {
        // `validate_origin_and_method` already rejected any non-matching
        // origin before forwarding; reachable only in a defensive sense.
        return;
    };

    if let Ok(value) = HeaderValue::from_str(&allow_origin) {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }

    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }

    // @cpt-begin:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-return-response
    // The response now carries every documented CORS header this feature
    // adds; `crate::proxy::engine` returns it, unmodified further by this
    // feature, to the browser.
    if cors.allow_credentials {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-return-response
}
// @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-add-headers
// @cpt-end:cpt-cf-oagw-flow-cors-browser-actual-cross-origin-request:p2:inst-cors-actual-flow-forward

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::upstream::Sharing;
    use axum::body::to_bytes;
    use axum::http::HeaderValue;

    fn enabled_cors() -> CorsConfig {
        CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec![CorsMethod::Get, CorsMethod::Post],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    fn headers_with_origin(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        headers
    }

    #[test]
    fn preflight_detected_and_answered_with_204_without_touching_config() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        headers.insert(
            header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        headers.insert(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            HeaderValue::from_static("Content-Type, Authorization"),
        );

        let outcome = preflight_fast_path(&Method::OPTIONS, &headers);
        let CorsOutcome::ShortCircuit(response) = outcome else {
            panic!("expected a short-circuited preflight response");
        };
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .unwrap(),
            "POST"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .unwrap(),
            "Content-Type, Authorization"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_MAX_AGE)
                .unwrap(),
            "86400"
        );
        assert_eq!(
            response.headers().get(header::VARY).unwrap(),
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
        );
        assert_eq!(
            response.headers().get(ERROR_SOURCE_HEADER_NAME).unwrap(),
            ERROR_SOURCE_GATEWAY
        );
    }

    #[test]
    fn preflight_without_access_control_request_headers_omits_allow_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        headers.insert(
            header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("GET"),
        );

        let outcome = preflight_fast_path(&Method::OPTIONS, &headers);
        let CorsOutcome::ShortCircuit(response) = outcome else {
            panic!("expected a short-circuited preflight response");
        };
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .is_none()
        );
    }

    #[test]
    fn options_without_both_headers_is_not_a_preflight() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        assert!(matches!(
            preflight_fast_path(&Method::OPTIONS, &headers),
            CorsOutcome::Continue
        ));

        assert!(matches!(
            preflight_fast_path(&Method::GET, &HeaderMap::new()),
            CorsOutcome::Continue
        ));
    }

    #[test]
    fn disabled_by_default_means_no_validation_applies() {
        assert!(matches!(
            validate_origin_and_method(
                None,
                &headers_with_origin("https://evil.com"),
                &Method::GET
            ),
            CorsOutcome::Continue
        ));

        let mut disabled = enabled_cors();
        disabled.enabled = false;
        assert!(matches!(
            validate_origin_and_method(
                Some(&disabled),
                &headers_with_origin("https://evil.com"),
                &Method::GET
            ),
            CorsOutcome::Continue
        ));
    }

    #[test]
    fn no_origin_header_skips_validation_even_when_enabled() {
        let cors = enabled_cors();
        assert!(matches!(
            validate_origin_and_method(Some(&cors), &HeaderMap::new(), &Method::GET),
            CorsOutcome::Continue
        ));
    }

    #[test]
    fn allowed_origin_and_method_continues() {
        let cors = enabled_cors();
        let headers = headers_with_origin("https://app.example.com");
        assert!(matches!(
            validate_origin_and_method(Some(&cors), &headers, &Method::GET),
            CorsOutcome::Continue
        ));
    }

    #[tokio::test]
    async fn disallowed_origin_rejected_with_documented_403_and_gateway_source() {
        let cors = enabled_cors();
        let headers = headers_with_origin("https://evil.com");
        let outcome = validate_origin_and_method(Some(&cors), &headers, &Method::GET);
        let CorsOutcome::ShortCircuit(response) = outcome else {
            panic!("expected a 403 rejection");
        };
        assert_eq!(response.status().as_u16(), 403);
        assert_eq!(
            response.headers().get(ERROR_SOURCE_HEADER_NAME).unwrap(),
            ERROR_SOURCE_GATEWAY
        );
        assert_eq!(response.headers().get(header::VARY).unwrap(), "Origin");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
    }

    #[tokio::test]
    async fn disallowed_method_rejected_with_documented_403_and_gateway_source() {
        let cors = enabled_cors();
        let headers = headers_with_origin("https://app.example.com");
        let outcome = validate_origin_and_method(Some(&cors), &headers, &Method::DELETE);
        let CorsOutcome::ShortCircuit(response) = outcome else {
            panic!("expected a 403 rejection");
        };
        assert_eq!(response.status().as_u16(), 403);
        assert_eq!(
            response.headers().get(ERROR_SOURCE_HEADER_NAME).unwrap(),
            ERROR_SOURCE_GATEWAY
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
    }

    #[test]
    fn http_origin_is_matched_like_any_other_scheme() {
        let mut cors = enabled_cors();
        cors.allowed_origins = vec!["http://localhost:3000".to_owned()];
        assert_eq!(
            match_origin("http://localhost:3000", &cors),
            Some("http://localhost:3000".to_owned())
        );
        assert_eq!(match_origin("https://localhost:3000", &cors), None);
        assert_eq!(match_origin("http://localhost:3001", &cors), None);
    }

    #[test]
    fn credentialed_wildcard_never_matches_via_the_wildcard_entry() {
        let mut cors = enabled_cors();
        cors.allow_credentials = true;
        cors.allowed_origins = vec!["*".to_owned(), "https://app.example.com".to_owned()];
        assert_eq!(match_origin("https://evil.com", &cors), None);
        assert_eq!(
            match_origin("https://app.example.com", &cors),
            Some("https://app.example.com".to_owned())
        );
    }

    #[test]
    fn non_credentialed_wildcard_matches_any_origin_with_wildcard_value() {
        let mut cors = enabled_cors();
        cors.allowed_origins = vec!["*".to_owned()];
        assert_eq!(
            match_origin("https://anywhere.example", &cors),
            Some("*".to_owned())
        );
    }

    #[test]
    fn inject_response_headers_noop_when_disabled_or_missing() {
        let mut response = StatusCode::OK.into_response();
        inject_response_headers(None, &HeaderMap::new(), &mut response);
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
        assert!(response.headers().get(header::VARY).is_none());

        let mut disabled = enabled_cors();
        disabled.enabled = false;
        let mut response = StatusCode::OK.into_response();
        inject_response_headers(
            Some(&disabled),
            &headers_with_origin("https://app.example.com"),
            &mut response,
        );
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    #[test]
    fn inject_response_headers_adds_allow_origin_expose_headers_and_credentials() {
        let mut cors = enabled_cors();
        cors.expose_headers = vec!["X-Request-ID".to_owned()];
        cors.allow_credentials = true;
        cors.allowed_origins = vec!["https://app.example.com".to_owned()];

        let mut response = StatusCode::OK.into_response();
        inject_response_headers(
            Some(&cors),
            &headers_with_origin("https://app.example.com"),
            &mut response,
        );

        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .unwrap(),
            "X-Request-ID"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        assert_eq!(response.headers().get(header::VARY).unwrap(), "Origin");
    }

    #[test]
    fn vary_origin_present_even_when_request_carries_no_origin() {
        let cors = enabled_cors();
        let mut response = StatusCode::OK.into_response();
        inject_response_headers(Some(&cors), &HeaderMap::new(), &mut response);
        assert_eq!(response.headers().get(header::VARY).unwrap(), "Origin");
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    #[test]
    fn vary_origin_appended_to_an_existing_vary_value_without_duplication() {
        let mut headers = HeaderMap::new();
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        append_vary_origin(&mut headers);
        assert_eq!(
            headers.get(header::VARY).unwrap(),
            "Accept-Encoding, Origin"
        );
        append_vary_origin(&mut headers);
        assert_eq!(
            headers.get(header::VARY).unwrap(),
            "Accept-Encoding, Origin"
        );
    }
}
