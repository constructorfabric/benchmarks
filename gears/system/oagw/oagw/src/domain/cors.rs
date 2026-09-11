//! CORS handling, per ADR-0004.
//!
//! Realizes `cpt-cf-oagw-algo-tp-cors-preflight` and
//! `cpt-cf-oagw-algo-tp-cors-actual-request`.
//!
//! Origin matching is exact: scheme, host and port are all significant and no
//! pattern matching is performed.

use crate::domain::model::CorsConfig;

/// How long a preflight result may be cached, in seconds.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// The `Vary` value on a preflight response.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// A preflight request's inputs.
#[derive(Debug, Clone)]
pub struct Preflight<'a> {
    /// The `Origin` header.
    pub origin: &'a str,
    /// The `Access-Control-Request-Method` header.
    pub method: &'a str,
    /// The `Access-Control-Request-Headers` header, when present.
    pub headers: Option<&'a str>,
}

/// Whether a request is a CORS preflight: `OPTIONS` carrying both `Origin` and
/// `Access-Control-Request-Method`.
#[must_use]
pub fn is_preflight(method: &str, origin: Option<&str>, acrm: Option<&str>) -> bool {
    method.eq_ignore_ascii_case("OPTIONS") && origin.is_some() && acrm.is_some()
}

/// The headers a preflight response carries.
///
/// A preflight is answered before upstream resolution and without evaluating
/// auth or the plugin chain, so it echoes what was asked for.
// @cpt-begin:cpt-cf-oagw-dod-tp-cors-preflight:p1:inst-full
#[must_use]
pub fn preflight_headers(p: &Preflight<'_>) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("access-control-allow-origin", p.origin.to_owned()),
        ("access-control-allow-methods", p.method.to_owned()),
        ("access-control-max-age", PREFLIGHT_MAX_AGE.to_owned()),
        ("vary", PREFLIGHT_VARY.to_owned()),
    ];
    if let Some(h) = p.headers {
        out.push(("access-control-allow-headers", h.to_owned()));
    }
    out
}
// @cpt-end:cpt-cf-oagw-dod-tp-cors-preflight:p1:inst-full

/// Why an actual cross-origin request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorsRejection {
    /// The origin is not in `allowed_origins`.
    OriginNotAllowed,
    /// The method is not in `allowed_methods`.
    MethodNotAllowed,
}

impl CorsRejection {
    /// The distinct problem type for this rejection, per ADR-0004.
    #[must_use]
    pub const fn problem_type(self) -> &'static str {
        match self {
            Self::OriginNotAllowed => "cf.oagw.cors.origin_not_allowed.v1",
            Self::MethodNotAllowed => "cf.oagw.cors.method_not_allowed.v1",
        }
    }

    /// A human-readable detail naming which check failed.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::OriginNotAllowed => "the request origin is not permitted for this upstream",
            Self::MethodNotAllowed => "the request method is not permitted for this upstream",
        }
    }
}

/// Whether an origin is permitted. Matching is exact, with `*` as the only
/// wildcard.
#[must_use]
pub fn origin_allowed(cfg: &CorsConfig, origin: &str) -> bool {
    cfg.allowed_origins
        .iter()
        .any(|o| o == "*" || o == origin)
}

/// Evaluate an actual (non-preflight) cross-origin request.
///
/// Returns `Ok(response headers)` when the request may proceed.
///
/// # Errors
/// Returns the specific [`CorsRejection`] that applies.
// @cpt-begin:cpt-cf-oagw-dod-tp-cors-actual-request:p1:inst-full
pub fn evaluate_actual(
    cfg: &CorsConfig,
    origin: &str,
    method: &str,
) -> Result<Vec<(&'static str, String)>, CorsRejection> {
    if !origin_allowed(cfg, origin) {
        return Err(CorsRejection::OriginNotAllowed);
    }
    if !cfg
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
    {
        return Err(CorsRejection::MethodNotAllowed);
    }

    let mut out = vec![
        ("access-control-allow-origin", origin.to_owned()),
        ("vary", "Origin".to_owned()),
    ];
    if !cfg.expose_headers.is_empty() {
        out.push((
            "access-control-expose-headers",
            cfg.expose_headers.join(", "),
        ));
    }
    if cfg.allow_credentials {
        out.push(("access-control-allow-credentials", "true".to_owned()));
    }
    Ok(out)
}
// @cpt-end:cpt-cf-oagw-dod-tp-cors-actual-request:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Sharing;

    fn cfg(origins: &[&str], methods: &[&str], creds: bool) -> CorsConfig {
        CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            expose_headers: vec!["x-trace".to_owned()],
            allow_credentials: creds,
        }
    }

    #[test]
    fn preflight_detection_needs_all_three_signals() {
        assert!(is_preflight("OPTIONS", Some("https://a"), Some("GET")));
        assert!(is_preflight("options", Some("https://a"), Some("GET")));
        assert!(!is_preflight("GET", Some("https://a"), Some("GET")));
        assert!(!is_preflight("OPTIONS", None, Some("GET")));
        assert!(!is_preflight("OPTIONS", Some("https://a"), None));
    }

    #[test]
    fn preflight_echoes_the_request_and_sets_max_age_and_vary() {
        let h = preflight_headers(&Preflight {
            origin: "https://app.example",
            method: "PUT",
            headers: Some("x-a, x-b"),
        });
        let get = |k: &str| {
            h.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("access-control-allow-origin"), "https://app.example");
        assert_eq!(get("access-control-allow-methods"), "PUT");
        assert_eq!(get("access-control-allow-headers"), "x-a, x-b");
        assert_eq!(get("access-control-max-age"), "86400");
        assert_eq!(get("vary"), PREFLIGHT_VARY);
    }

    #[test]
    fn preflight_omits_allow_headers_when_none_were_requested() {
        let h = preflight_headers(&Preflight {
            origin: "https://a",
            method: "GET",
            headers: None,
        });
        assert!(!h.iter().any(|(n, _)| *n == "access-control-allow-headers"));
    }

    #[test]
    fn origin_matching_is_exact_and_port_sensitive() {
        let c = cfg(&["https://app.example"], &["GET"], false);
        assert!(origin_allowed(&c, "https://app.example"));
        assert!(!origin_allowed(&c, "https://app.example:8443"));
        assert!(!origin_allowed(&c, "http://app.example"));
        assert!(!origin_allowed(&c, "https://evil.example"));
        // No suffix matching: the classic bypass shape is refused.
        assert!(!origin_allowed(&c, "https://app.example.evil.test"));
    }

    #[test]
    fn wildcard_origin_admits_anything() {
        let c = cfg(&["*"], &["GET"], false);
        assert!(origin_allowed(&c, "https://whatever.example"));
    }

    #[test]
    fn a_disallowed_origin_and_method_are_distinct_rejections() {
        let c = cfg(&["https://a.example"], &["GET"], false);
        assert_eq!(
            evaluate_actual(&c, "https://b.example", "GET").unwrap_err(),
            CorsRejection::OriginNotAllowed
        );
        assert_eq!(
            evaluate_actual(&c, "https://a.example", "DELETE").unwrap_err(),
            CorsRejection::MethodNotAllowed
        );
        assert_ne!(
            CorsRejection::OriginNotAllowed.problem_type(),
            CorsRejection::MethodNotAllowed.problem_type()
        );
    }

    #[test]
    fn an_allowed_request_gains_the_documented_response_headers() {
        let c = cfg(&["https://a.example"], &["GET"], true);
        let h = evaluate_actual(&c, "https://a.example", "get").unwrap();
        let names: Vec<&str> = h.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"access-control-allow-origin"));
        assert!(names.contains(&"vary"));
        assert!(names.contains(&"access-control-expose-headers"));
        assert!(names.contains(&"access-control-allow-credentials"));
    }

    #[test]
    fn credentials_header_is_absent_when_not_configured() {
        let c = cfg(&["https://a.example"], &["GET"], false);
        let h = evaluate_actual(&c, "https://a.example", "GET").unwrap();
        assert!(
            !h.iter()
                .any(|(n, _)| *n == "access-control-allow-credentials")
        );
    }
}
