// Created: 2026-09-04 by Constructor Tech
//! Built-in CORS handling of the data plane
//! (`docs/ADR/0004-cors.md`).
//!
//! A preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered locally with a permissive `204` and never reaches an upstream;
//! origin and method validation happens on the actual cross-origin request,
//! after upstream resolution and before forwarding.

use http::{HeaderMap, HeaderValue, Method};

use crate::domain::{AllowedOrigin, CorsConfig, HttpMethod};

/// `Access-Control-Max-Age` of a preflight response (`docs/ADR/0004-cors.md`).
pub const MAX_AGE_SECS: u64 = 86_400;

/// `Vary` value of a preflight response.
pub const VARY_PREFLIGHT: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// `Vary` value of an actual cross-origin response.
pub const VARY_ACTUAL: &str = "Origin";

/// Why an actual cross-origin request was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsRejection {
    /// The origin is not in `allowed_origins`.
    Origin {
        /// Rejected origin.
        origin: String,
    },
    /// The method is not in `allowed_methods`.
    Method {
        /// Rejected method token.
        method: String,
    },
}

/// `true` when `method` and `headers` make the request a CORS preflight.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// `true` when `method` is the `OPTIONS` of a CORS preflight.
///
/// `OPTIONS` is not a route-matching method, so a preflight cannot be matched
/// by the method allowlist of a route; the route lookup of the proxy uses this
/// predicate to match a preflight on the path alone.
#[must_use]
pub const fn is_preflight_method(method: HttpMethod) -> bool {
    matches!(method, HttpMethod::Options)
}

/// `true` when `origin` is in the configured allowlist.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    let lowered = origin.to_ascii_lowercase();
    config.allowed_origins.iter().any(|allowed| match allowed {
        AllowedOrigin::Any => true,
        AllowedOrigin::Exact(exact) => exact.to_ascii_lowercase() == lowered,
    })
}

/// `true` when `method` is in the configured allowlist; an unknown method
/// token is never allowed.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &Method) -> bool {
    HttpMethod::parse(method.as_str()).is_ok_and(|parsed| config.allowed_methods.contains(&parsed))
}

/// Validates an actual (non-preflight) cross-origin request.
///
/// Same-origin requests — those without an `Origin` header — are always
/// allowed.
///
/// # Errors
///
/// Returns [`CorsRejection::Origin`] or [`CorsRejection::Method`] for a
/// cross-origin request outside the configured allowlist.
pub fn check_actual(
    config: &CorsConfig,
    origin: Option<&str>,
    method: &Method,
) -> Result<(), CorsRejection> {
    let Some(origin) = origin else {
        return Ok(());
    };
    if !origin_allowed(config, origin) {
        return Err(CorsRejection::Origin {
            origin: origin.to_owned(),
        });
    }
    if !method_allowed(config, method) {
        return Err(CorsRejection::Method {
            method: method.as_str().to_owned(),
        });
    }
    Ok(())
}

/// Headers of the permissive preflight answer
/// (`docs/ADR/0004-cors.md` "Response headers (preflight)").
#[must_use]
pub fn preflight_headers(
    origin: &str,
    requested_method: &str,
    requested_headers: Option<&str>,
) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    set(
        &mut headers,
        http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        origin,
    );
    set(
        &mut headers,
        http::header::ACCESS_CONTROL_ALLOW_METHODS,
        requested_method,
    );
    if let Some(requested) = requested_headers {
        set(
            &mut headers,
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            requested,
        );
    }
    set(
        &mut headers,
        http::header::ACCESS_CONTROL_MAX_AGE,
        &MAX_AGE_SECS.to_string(),
    );
    set(&mut headers, http::header::VARY, VARY_PREFLIGHT);
    headers
}

/// Adds the CORS headers of an actual cross-origin response to `headers`
/// (`docs/ADR/0004-cors.md` "Response headers (actual request)").
pub fn write_response_headers(config: &CorsConfig, origin: &str, headers: &mut http::HeaderMap) {
    let allow_origin = if config.allow_credentials {
        origin.to_owned()
    } else if config
        .allowed_origins
        .iter()
        .any(|allowed| matches!(allowed, AllowedOrigin::Any))
    {
        String::from("*")
    } else {
        origin.to_owned()
    };
    set(
        headers,
        http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        &allow_origin,
    );
    if !config.expose_headers.is_empty() {
        let exposed = config.expose_headers.join(", ");
        set(
            headers,
            http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
            &exposed,
        );
    }
    if config.allow_credentials {
        set(
            headers,
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            "true",
        );
    }
    set(headers, http::header::VARY, VARY_ACTUAL);
}

/// Inserts a header value, ignoring values the HTTP grammar refuses.
fn set(headers: &mut http::HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "cors_tests.rs"]
mod cors_tests;
