//! Built-in CORS handler (`docs/ADR/0004-cors.md`).
//!
//! Two phases, handled by the gateway itself rather than by a guard plugin:
//!
//! * **Preflight** (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//!   answered locally with a *permissive* 204 that echoes the requested
//!   origin, method and headers. No upstream is dialed and no tenant context
//!   is needed; origin enforcement happens on the actual request.
//! * **Actual request** — the origin and the method are validated against the
//!   effective CORS configuration after upstream resolution and before the
//!   upstream is dialed, and the response carries the CORS headers plus
//!   `Vary: Origin`.
//!
//! `allow_credentials` combined with a wildcard origin is rejected at
//! configuration time (`CorsConfig::validate`), so this module only has to
//! honour the invariant.
use crate::domain::model::CorsConfig;
use crate::infra::proxy::failure::ProxyFailure;

/// `Access-Control-Allow-Origin`.
pub const ALLOW_ORIGIN: &str = "access-control-allow-origin";
/// `Access-Control-Allow-Methods`.
pub const ALLOW_METHODS: &str = "access-control-allow-methods";
/// `Access-Control-Allow-Headers`.
pub const ALLOW_HEADERS: &str = "access-control-allow-headers";
/// `Access-Control-Allow-Credentials`.
pub const ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
/// `Access-Control-Expose-Headers`.
pub const EXPOSE_HEADERS: &str = "access-control-expose-headers";
/// `Access-Control-Max-Age`.
pub const MAX_AGE: &str = "access-control-max-age";
/// `Access-Control-Request-Method`.
pub const REQUEST_METHOD: &str = "access-control-request-method";
/// `Access-Control-Request-Headers`.
pub const REQUEST_HEADERS: &str = "access-control-request-headers";
/// `Vary`.
pub const VARY: &str = "vary";
/// `Origin`.
pub const ORIGIN: &str = "origin";

/// How long a preflight answer may be cached.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// Shape of the inbound request, as far as CORS is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsRequest {
    /// Request method.
    pub method: String,
    /// `Origin` header value, when the request carries one.
    pub origin: Option<String>,
    /// `Access-Control-Request-Method`, preflight only.
    pub request_method: Option<String>,
    /// `Access-Control-Request-Headers`, comma-separated.
    pub request_headers: Option<String>,
}

/// Verdict for one request.
#[derive(Debug)]
pub enum CorsOutcome {
    /// Not a CORS request; nothing to do.
    NotApplicable,
    /// Answer the preflight locally with these headers and status 204.
    Preflight(Vec<(&'static str, String)>),
    /// Let the hop proceed and add these headers to the response.
    Allowed(Vec<(&'static str, String)>),
    /// Reject the actual request with a 403 problem document.
    Rejected(ProxyFailure),
}

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(request: &CorsRequest) -> bool {
    request.method.eq_ignore_ascii_case("OPTIONS")
        && request.origin.is_some()
        && request.request_method.is_some()
}

/// Whether the request is cross-origin at all.
#[must_use]
fn is_cross_origin(request: &CorsRequest) -> bool {
    request
        .origin
        .as_deref()
        .map(str::trim)
        .is_some_and(|origin| !origin.is_empty())
}

/// Evaluate CORS for a request against an effective configuration.
///
/// A request without an `Origin` header is never a CORS request. A disabled or
/// absent CORS policy makes the request a plain proxy hop, which also means a
/// cross-origin request is *not* rejected — `docs/ADR/0004` ("CORS disabled
/// unless explicitly enabled").
#[must_use]
pub fn evaluate(config: Option<&CorsConfig>, request: &CorsRequest) -> CorsOutcome {
    if !is_cross_origin(request) {
        return CorsOutcome::NotApplicable;
    }
    let Some(config) = config.filter(|config| config.enabled) else {
        return CorsOutcome::NotApplicable;
    };
    if is_preflight(request) {
        return CorsOutcome::Preflight(preflight_headers(config, request));
    }
    let origin = request.origin.clone().unwrap_or_default();
    if !origin_allowed(config, &origin) {
        return CorsOutcome::Rejected(ProxyFailure::new(
            403,
            crate::domain::plugin::CORS_ORIGIN_NOT_ALLOWED,
            "CORS Origin Not Allowed",
            format!("origin '{origin}' not in allowed origins list"),
        ));
    }
    if !method_allowed(config, &request.method) {
        return CorsOutcome::Rejected(ProxyFailure::new(
            403,
            crate::domain::plugin::CORS_METHOD_NOT_ALLOWED,
            "CORS Method Not Allowed",
            format!("method '{}' not in allowed methods list", request.method),
        ));
    }
    CorsOutcome::Allowed(actual_headers(config, &origin))
}

/// The permissive preflight answer, with no configuration to read.
///
/// `docs/ADR/0004` pins the preflight ahead of upstream resolution — a browser
/// sends it without credentials, so there is no tenant context — and has it
/// echo the requested origin, method and headers. Origin and method are
/// enforced on the actual request that follows.
#[must_use]
pub fn preflight_reply(request: &CorsRequest) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (ALLOW_ORIGIN, request.origin.clone().unwrap_or_default()),
        (
            ALLOW_METHODS,
            request.request_method.clone().unwrap_or_default(),
        ),
        (
            ALLOW_HEADERS,
            request
                .request_headers
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        ),
        (MAX_AGE, PREFLIGHT_MAX_AGE.to_owned()),
        (
            VARY,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
        ),
    ];
    headers.retain(|(_, value)| !value.is_empty() || value == &PREFLIGHT_MAX_AGE.to_owned());
    headers
}

/// Preflight answer: echo the requested origin, method and headers.
fn preflight_headers(config: &CorsConfig, request: &CorsRequest) -> Vec<(&'static str, String)> {
    let origin = request.origin.clone().unwrap_or_default();
    let mut headers = vec![
        (ALLOW_ORIGIN, origin),
        (
            ALLOW_METHODS,
            request.request_method.clone().unwrap_or_default(),
        ),
        (
            ALLOW_HEADERS,
            request
                .request_headers
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        ),
        (MAX_AGE, PREFLIGHT_MAX_AGE.to_owned()),
        (
            VARY,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
        ),
    ];
    if config.allow_credentials {
        headers.push((ALLOW_CREDENTIALS, "true".to_owned()));
    }
    headers
}

/// Headers added to a forwarded cross-origin response.
fn actual_headers(config: &CorsConfig, origin: &str) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (ALLOW_ORIGIN, origin.to_owned()),
        (VARY, "Origin".to_owned()),
    ];
    if config.allow_credentials {
        headers.push((ALLOW_CREDENTIALS, "true".to_owned()));
    }
    if !config.expose_headers.is_empty() {
        headers.push((EXPOSE_HEADERS, config.expose_headers.join(", ")));
    }
    headers
}

/// Exact, case-insensitive, port- and protocol-sensitive origin matching.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// Whether the request method is in the configured allowlist.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &str) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

#[cfg(test)]
#[path = "cors_tests.rs"]
mod tests;
