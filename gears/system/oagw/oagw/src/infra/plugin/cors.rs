//! CORS guard identifier — **catalog identifier only**
//! (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1`).
//!
//! CORS is *core Data Plane logic configured via the dedicated `cors` field on
//! `Upstream`/`Route`* (ADR 0004), not a `GuardPlugin` trait implementation:
//! the preflight fast path must answer without running the plugin chain. The
//! identifier exists for types-registry cataloging only and is deliberately
//! **not** resolvable via
//! [`GuardPluginRegistry`](crate::infra::plugin::GuardPluginRegistry).

pub use crate::domain::gts_helpers::CORS_GUARD_PLUGIN_ID;
use crate::domain::model::CorsConfig;

/// Preflight request method.
pub const CORS_PREFLIGHT_METHOD: &str = "OPTIONS";

/// Access-control headers the data plane emits.
pub mod headers {
    /// `Access-Control-Allow-Origin`
    pub const ALLOW_ORIGIN: &str = "access-control-allow-origin";
    /// `Access-Control-Allow-Methods`
    pub const ALLOW_METHODS: &str = "access-control-allow-methods";
    /// `Access-Control-Allow-Headers`
    pub const ALLOW_HEADERS: &str = "access-control-allow-headers";
    /// `Access-Control-Allow-Credentials`
    pub const ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
    /// `Access-Control-Expose-Headers`
    pub const EXPOSE_HEADERS: &str = "access-control-expose-headers";
    /// `Access-Control-Max-Age`
    pub const MAX_AGE: &str = "access-control-max-age";
    /// `Vary` (set on preflight responses so caches key on the CORS inputs).
    pub const VARY: &str = "vary";
    /// `Origin` (request header)
    pub const ORIGIN: &str = "origin";
    /// `Access-Control-Request-Method` (preflight request header)
    pub const REQUEST_METHOD: &str = "access-control-request-method";
    /// `Access-Control-Request-Headers` (preflight request header)
    pub const REQUEST_HEADERS: &str = "access-control-request-headers";
}

/// Decided CORS treatment of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsDecision {
    /// Not a CORS request (no `Origin` header): no CORS headers to add.
    NotApplicable,
    /// Allowed: the listed headers must be added to the response.
    Allow(Vec<(String, String)>),
    /// Refused: the origin is not allowed.
    Reject,
}

/// `Access-Control-Max-Age` for preflight responses (ADR 0004).
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// The `Vary` header value preflight responses carry.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// True when the request is a CORS preflight (ADR 0004): `OPTIONS` with an
/// `Origin` and an `Access-Control-Request-Method` header.
#[must_use]
pub fn is_preflight(method: &str, origin: Option<&str>, request_method: Option<&str>) -> bool {
    method.eq_ignore_ascii_case(CORS_PREFLIGHT_METHOD)
        && origin.is_some_and(|o| !o.trim().is_empty())
        && request_method.is_some_and(|m| !m.trim().is_empty())
}

/// Build the *permissive* preflight response (ADR 0004).
///
/// A preflight carries no credentials, so no tenant context is available and
/// no upstream resolution happens: the requested origin, method and headers are
/// echoed back. Origin and method enforcement is deferred to the actual
/// request, which *does* have tenant context.
#[must_use]
pub fn preflight_response(
    origin: &str,
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> Vec<(String, String)> {
    let mut out = vec![
        (headers::ALLOW_ORIGIN.to_owned(), origin.to_owned()),
        (
            headers::ALLOW_METHODS.to_owned(),
            request_method.unwrap_or_default().trim().to_owned(),
        ),
        (headers::MAX_AGE.to_owned(), PREFLIGHT_MAX_AGE.to_owned()),
        (headers::VARY.to_owned(), PREFLIGHT_VARY.to_owned()),
    ];
    if let Some(requested_headers) = request_headers {
        let echoed = request_headers_value(requested_headers);
        if !echoed.is_empty() {
            out.push((headers::ALLOW_HEADERS.to_owned(), echoed));
        }
    }
    out
}

/// Echo the requested header list for `Access-Control-Allow-Headers`.
fn request_headers_value(raw: &str) -> String {
    let names: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    names.join(", ")
}

/// Evaluate a CORS configuration against an *actual* request.
///
/// Pure function so the data plane can use it on the preflight fast path
/// without constructing plugin contexts.
#[must_use]
pub fn decide(cors: &CorsConfig, origin: Option<&str>, is_preflight: bool) -> CorsDecision {
    if is_preflight {
        // Handled by `preflight_response` before any upstream resolution.
        return CorsDecision::NotApplicable;
    }
    if !cors.enabled {
        return CorsDecision::NotApplicable;
    }
    let Some(origin) = origin else {
        return CorsDecision::NotApplicable;
    };
    if !origin_allowed(cors, origin) {
        return CorsDecision::Reject;
    }
    let mut out = vec![(
        headers::ALLOW_ORIGIN.to_owned(),
        if cors.allowed_origins.iter().any(|o| o == "*") {
            "*".to_owned()
        } else {
            origin.to_owned()
        },
    )];
    if cors.allow_credentials {
        out.push((headers::ALLOW_CREDENTIALS.to_owned(), "true".to_owned()));
    }
    if !cors.expose_headers.is_empty() {
        out.push((
            headers::EXPOSE_HEADERS.to_owned(),
            cors.expose_headers.join(", "),
        ));
    }
    CorsDecision::Allow(out)
}

/// True when `method` is served for cross-origin requests.
///
/// A configuration that lists no methods falls back to the JSON-schema
/// default (`GET`, `POST`).
#[must_use]
pub fn method_allowed(cors: &CorsConfig, method: &str) -> bool {
    if !cors.enabled {
        return true;
    }
    if cors.allowed_methods.is_empty() {
        return crate::domain::model::DEFAULT_CORS_METHODS
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method));
    }
    cors.allowed_methods
        .iter()
        .any(|m| m.trim().eq_ignore_ascii_case(method))
}

/// True when `origin` is allowed by `cors` (exact match, or `["*"]`).
#[must_use]
pub fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    if cors.allowed_origins.is_empty() {
        return false;
    }
    cors.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::registry::GuardPluginRegistry;

    #[test]
    fn identifier_is_catalog_only() {
        assert!(
            GuardPluginRegistry::with_builtins()
                .get(CORS_GUARD_PLUGIN_ID)
                .is_none()
        );
    }

    #[test]
    fn disabled_cors_is_not_applicable() {
        let cors = CorsConfig::default();
        assert_eq!(
            decide(&cors, Some("https://app.example.com"), false),
            CorsDecision::NotApplicable
        );
    }

    #[test]
    fn wildcard_origin_is_matched() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            ..CorsConfig::default()
        };
        assert!(origin_allowed(&cors, "https://anything.example.net"));
        match decide(&cors, Some("https://anything.example.net"), false) {
            CorsDecision::Allow(hs) => {
                // A wildcard origin is echoed as `*` on an actual request.
                assert!(hs.contains(&(headers::ALLOW_ORIGIN.to_owned(), "*".to_owned())));
                // `Access-Control-Allow-Methods` is a preflight-only header.
                assert!(hs.iter().all(|(k, _)| k != headers::ALLOW_METHODS));
            }
            other => panic!("expected allow, got {other:?}"),
        }
    }

    #[test]
    fn unknown_origin_is_rejected() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert_eq!(
            decide(&cors, Some("https://evil.example.net"), false),
            CorsDecision::Reject
        );
        match decide(&cors, Some("https://app.example.com"), false) {
            CorsDecision::Allow(hs) => {
                assert!(
                    hs.iter()
                        .any(|(k, v)| k == headers::ALLOW_ORIGIN && v == "https://app.example.com")
                );
                assert!(hs.iter().any(|(k, _)| k == headers::ALLOW_CREDENTIALS));
            }
            other => panic!("expected allow, got {other:?}"),
        }
    }
}
