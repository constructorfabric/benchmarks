//! Cross-Origin Resource Sharing (ADR 0004 "CORS").
//!
//! Two request classes are distinguished:
//!
//! * **Preflight** — `OPTIONS` carrying `Origin` and
//!   `Access-Control-Request-Method`. It is answered locally with `204 No
//!   Content`, echoing the requested origin/method/headers. No upstream
//!   resolution, no tenant context and no auth plugin runs.
//! * **Actual cross-origin requests** — validated against the effective
//!   `cors` block of the resolved upstream. A rejected origin or method
//!   answers `403` with `cf.oagw.cors.origin_not_allowed.v1` /
//!   `cf.oagw.cors.method_not_allowed.v1`.
//!
//! Origin matching is exact and port/protocol sensitive — no wildcards other
//! than the single entry `*`, and no regular expressions.

use crate::domain::error::{CorsRejection, DomainError};
use crate::domain::models::{CorsConfig, CorsMethod};

/// Header carrying the caller's origin.
pub const ORIGIN_HEADER: &str = "origin";
/// Header carrying the preflight request method.
pub const ACCESS_CONTROL_REQUEST_METHOD: &str = "access-control-request-method";
/// Header carrying the preflight request headers.
pub const ACCESS_CONTROL_REQUEST_HEADERS: &str = "access-control-request-headers";
/// Preflight cache lifetime advertised by OAGW.
pub const ACCESS_CONTROL_MAX_AGE: &str = "86400";

/// Classification of an inbound request for CORS purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsRequest {
    /// Not a cross-origin request (no `Origin` header) — CORS is inert.
    SameOrigin,
    /// Browser preflight, answered by the gateway.
    Preflight {
        /// Origin the preflight asks for.
        origin: String,
        /// Method the preflight asks about.
        request_method: String,
        /// Headers the preflight asks about.
        request_headers: Vec<String>,
    },
    /// An actual cross-origin request that must be validated.
    Actual { origin: String },
}

/// Classifies an inbound request from its method and CORS headers.
#[must_use]
pub fn classify(method: &str, origin: Option<&str>, request_method: Option<&str>) -> CorsRequest {
    let Some(origin) = origin.map(str::trim).filter(|origin| !origin.is_empty()) else {
        return CorsRequest::SameOrigin;
    };
    let is_options = method.eq_ignore_ascii_case("OPTIONS");
    let has_request_method = request_method
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if is_options && has_request_method {
        CorsRequest::Preflight {
            origin: origin.to_owned(),
            request_method: request_method.unwrap_or_default().trim().to_owned(),
            request_headers: Vec::new(),
        }
    } else {
        CorsRequest::Actual {
            origin: origin.to_owned(),
        }
    }
}

/// Whether `origin` is admitted by `config`.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// Whether `method` is listed in the CORS method policy.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &str) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| cors_method_label(*allowed).eq_ignore_ascii_case(method))
}

/// Validates an actual cross-origin request against the effective policy.
///
/// # Errors
///
/// Returns [`DomainError::CorsForbidden`] when the origin or the method is not
/// allowed by the policy.
pub fn check_actual_request(
    config: Option<&CorsConfig>,
    request: &CorsRequest,
    method: &str,
) -> Result<(), DomainError> {
    let CorsRequest::Actual { origin } = request else {
        return Ok(());
    };
    let Some(config) = config.filter(|config| config.enabled) else {
        return Ok(());
    };
    if !origin_allowed(config, origin) {
        return Err(DomainError::CorsForbidden {
            reason: CorsRejection::Origin,
            detail: format!("origin '{origin}' is not allowed by the CORS policy"),
            origin: Some(origin.to_owned()),
        });
    }
    if !method_allowed(config, method) {
        return Err(DomainError::CorsForbidden {
            reason: CorsRejection::Method,
            detail: format!("method '{method}' is not allowed by the CORS policy"),
            origin: Some(origin.to_owned()),
        });
    }
    Ok(())
}

/// Appends the CORS response headers for an actual cross-origin request.
pub fn apply_response_headers(config: Option<&CorsConfig>, request: &CorsRequest, headers: &mut Vec<(String, String)>) {
    let CorsRequest::Actual { origin } = request else {
        return;
    };
    let Some(config) = config.filter(|config| config.enabled) else {
        return;
    };
    if !origin_allowed(config, origin) {
        return;
    }
    set_or_push(headers, "access-control-allow-origin", origin);
    if config.allow_credentials {
        set_or_push(headers, "access-control-allow-credentials", "true");
    }
    if !config.expose_headers.is_empty() {
        set_or_push(
            headers,
            "access-control-expose-headers",
            &config.expose_headers.join(", "),
        );
    }
    set_or_push(headers, "vary", "Origin");
}

/// Lowers a configured CORS method to its wire label.
#[must_use]
pub fn cors_method_label(method: crate::domain::models::CorsMethod) -> &'static str {
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

fn set_or_push(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
    {
        value.clone_into(&mut slot.1);
    } else {
        headers.push((name.to_owned(), value.to_owned()));
    }
}

#[cfg(test)]
#[path = "cors_tests.rs"]
mod tests;
