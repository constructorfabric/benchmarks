//! CORS matching and validation (ADR 0004).
//!
//! Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered permissively at the handler level — no upstream resolution, no
//! tenant context. Actual cross-origin requests are validated here, after
//! upstream resolution and before forwarding.

use crate::domain::dto::Cors;

/// The `Access-Control-Max-Age` a permissive preflight advertises.
pub const PREFLIGHT_MAX_AGE: u64 = 86_400;

/// The headers a preflight response echoes.
pub const VARY_HEADER: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// Whether the request is a CORS preflight.
pub fn is_preflight(method: &str, origin: Option<&str>, request_method: Option<&str>) -> bool {
    method.eq_ignore_ascii_case("OPTIONS")
        && origin.is_some()
        && request_method.is_some()
}

/// Whether the request is a cross-origin request at all.
pub fn is_cross_origin(origin: Option<&str>) -> bool {
    origin.is_some()
}

/// Normalises an origin for comparison: lowercase scheme and host, drop the
/// default port, strip a trailing slash.
pub fn normalise_origin(origin: &str) -> String {
    let mut origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    // `https://example.com:443` == `https://example.com`
    if let Some(rest) = origin
        .strip_prefix("https://")
        .map(|r| r.to_string())
    {
        origin = format!("https://{}", rest.trim_end_matches(":443"));
    } else if let Some(rest) = origin
        .strip_prefix("http://")
        .map(|r| r.to_string())
    {
        origin = format!("http://{}", rest.trim_end_matches(":80"));
    }
    origin
}

/// Whether `origin` is allowed by the policy.
///
/// Matching is exact and port- and protocol-sensitive (`https://a.com` is not
/// `http://a.com` and not `https://a.com:8443`), except for the `*` wildcard
/// which allows every origin.
pub fn origin_allowed(cors: &Cors, origin: &str) -> bool {
    if !cors.enabled {
        return false;
    }
    if cors.allowed_origins.iter().any(|o| o == "*") {
        return true;
    }
    let candidate = normalise_origin(origin);
    cors.allowed_origins
        .iter()
        .any(|allowed| normalise_origin(allowed) == candidate)
}

/// Whether `method` is allowed by the policy.
pub fn method_allowed(cors: &Cors, method: &str) -> bool {
    if !cors.enabled {
        return false;
    }
    cors.allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
}

/// Whether the policy allows credentials together with the wildcard origin.
///
/// The wire schema rejects the combination at create time; this guard keeps
/// the runtime honest if a policy is assembled programmatically.
pub fn is_consistent(cors: &Cors) -> Result<(), String> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(
            "allow_credentials requires specific origins and cannot be combined with `*`"
                .to_string(),
        );
    }
    Ok(())
}

/// The outcome of a preflight evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightOutcome {
    /// Whether the preflight is allowed at all.
    pub allowed: bool,
    /// Value for `Access-Control-Allow-Origin`, when allowed.
    pub allow_origin: Option<String>,
    /// Value for `Access-Control-Allow-Methods`, when allowed.
    pub allow_methods: Option<String>,
    /// Value for `Access-Control-Allow-Headers`, when allowed.
    pub allow_headers: Option<String>,
    /// Value for `Access-Control-Allow-Credentials`.
    pub allow_credentials: bool,
    /// Value for `Access-Control-Expose-Headers`, when any.
    pub expose_headers: Option<String>,
    /// Value for `Access-Control-Max-Age`.
    pub max_age: u64,
    /// The `Vary` header a preflight echoes.
    pub vary: &'static str,
}

impl PreflightOutcome {
    /// The permissive preflight ADR 0004 answers without resolving the
    /// upstream: the requested origin, method and headers are echoed and
    /// enforcement is deferred to the actual request that follows.
    pub fn permissive(origin: Option<&str>, method: Option<&str>, headers: Option<&str>) -> Self {
        Self {
            allowed: true,
            allow_origin: Some(origin.unwrap_or("*").to_string()),
            allow_methods: Some(
                method
                    .map(str::to_string)
                    .unwrap_or_else(|| "GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS".to_string()),
            ),
            allow_headers: Some(headers.unwrap_or("*").to_string()),
            allow_credentials: false,
            expose_headers: None,
            max_age: PREFLIGHT_MAX_AGE,
            vary: VARY_HEADER,
        }
    }
}

/// Why an actual cross-origin request was refused (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsRejection {
    /// The origin is not in `allowed_origins`.
    Origin(String),
    /// The method is not in `allowed_methods`.
    Method(String),
}

impl CorsRejection {
    /// The value the problem body names as `invalidValue`.
    pub fn invalid_value(&self) -> &str {
        match self {
            CorsRejection::Origin(value) | CorsRejection::Method(value) => value,
        }
    }
}

/// Enforces a CORS policy on an actual request (ADR 0004).
///
/// A request with no `Origin` is same-origin by definition and CORS never
/// applies to it; an upstream with no policy, or with a disabled one, admits
/// no cross-origin request at all (deny by default).
pub fn enforce(policy: Option<&Cors>, origin: Option<&str>, method: &str) -> Result<(), CorsRejection> {
    let Some(origin) = origin else {
        return Ok(());
    };
    let Some(policy) = policy.filter(|p| p.enabled) else {
        return Err(CorsRejection::Origin(origin.to_string()));
    };
    if !origin_allowed(policy, origin) {
        return Err(CorsRejection::Origin(origin.to_string()));
    }
    if !method_allowed(policy, method) {
        return Err(CorsRejection::Method(method.to_string()));
    }
    Ok(())
}

/// The CORS headers an allowed actual request carries (ADR 0004).
pub fn response_headers(policy: Option<&Cors>, origin: &str) -> Vec<(String, String)> {
    let mut out = vec![("access-control-allow-origin".to_string(), origin.to_string())];
    if let Some(policy) = policy {
        if !policy.expose_headers.is_empty() {
            out.push((
                "access-control-expose-headers".to_string(),
                policy.expose_headers.join(", "),
            ));
        }
        if policy.allow_credentials {
            out.push((
                "access-control-allow-credentials".to_string(),
                "true".to_string(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors(origins: &[&str], methods: &[&str]) -> Cors {
        Cors {
            enabled: true,
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
            allowed_methods: methods.iter().map(|s| s.to_string()).collect(),
            ..Cors::default()
        }
    }

    #[test]
    fn preflight_is_detected_by_method_origin_and_request_method() {
        assert!(is_preflight(
            "OPTIONS",
            Some("https://a.com"),
            Some("POST")
        ));
        assert!(!is_preflight("GET", Some("https://a.com"), Some("POST")));
        assert!(!is_preflight("OPTIONS", None, Some("POST")));
        assert!(!is_preflight("OPTIONS", Some("https://a.com"), None));
    }

    #[test]
    fn origin_matching_is_exact() {
        let c = cors(&["https://app.example.com"], &["GET"]);
        assert!(origin_allowed(&c, "https://app.example.com"));
        // The ADR's documented rejects: a different host, a different port
        // and a different scheme are all refused.
        assert!(!origin_allowed(&c, "https://evil.com"));
        assert!(!origin_allowed(&c, "https://app.example.com:8080"));
        assert!(!origin_allowed(&c, "http://app.example.com"));
        // A trailing slash carries no information a browser would send.
        assert!(origin_allowed(&c, "https://app.example.com/"));
    }

    #[test]
    fn origin_matching_is_port_sensitive() {
        let c = cors(&["https://app.example.com:8443"], &["GET"]);
        assert!(origin_allowed(&c, "https://app.example.com:8443"));
        assert!(!origin_allowed(&c, "https://app.example.com"));
    }

    #[test]
    fn origin_matching_is_protocol_sensitive() {
        let c = cors(&["https://app.example.com"], &["GET"]);
        assert!(!origin_allowed(&c, "http://app.example.com"));
    }

    #[test]
    fn default_ports_are_equivalent() {
        let c = cors(&["https://app.example.com"], &["GET"]);
        assert!(origin_allowed(&c, "https://app.example.com:443"));
        let http = cors(&["http://app.example.com"], &["GET"]);
        assert!(origin_allowed(&http, "http://app.example.com:80"));
    }

    #[test]
    fn the_wildcard_allows_every_origin() {
        let c = cors(&["*"], &["GET"]);
        assert!(origin_allowed(&c, "https://anything.example"));
    }

    #[test]
    fn a_disabled_policy_allows_nothing() {
        let mut c = cors(&["*"], &["GET"]);
        c.enabled = false;
        assert!(!origin_allowed(&c, "https://anything.example"));
        assert!(!method_allowed(&c, "GET"));
    }

    #[test]
    fn method_matching_is_case_insensitive() {
        let c = cors(&["*"], &["GET", "POST"]);
        assert!(method_allowed(&c, "get"));
        assert!(!method_allowed(&c, "DELETE"));
    }

    #[test]
    fn credentials_and_the_wildcard_are_incompatible() {
        let mut c = cors(&["*"], &["GET"]);
        c.allow_credentials = true;
        assert!(is_consistent(&c).is_err());
        c.allowed_origins = vec!["https://app.example.com".into()];
        assert!(is_consistent(&c).is_ok());
    }

    #[test]
    fn a_preflight_is_answered_permissively_whatever_the_policy() {
        // ADR 0004: the preflight is a browser formality answered from the
        // request alone -- the origin and method it asks about are echoed, and
        // the policy is applied to the actual request that follows.
        let outcome = PreflightOutcome::permissive(
            Some("https://a.com"),
            Some("POST"),
            Some("Content-Type, Authorization"),
        );
        assert!(outcome.allowed);
        assert_eq!(outcome.allow_origin.as_deref(), Some("https://a.com"));
        assert_eq!(outcome.allow_methods.as_deref(), Some("POST"));
        assert_eq!(outcome.allow_headers.as_deref(), Some("Content-Type, Authorization"));
        assert_eq!(outcome.max_age, PREFLIGHT_MAX_AGE);
        assert_eq!(outcome.vary, VARY_HEADER);
    }

    #[test]
    fn a_preflight_without_a_request_method_is_still_permissive() {
        let outcome = PreflightOutcome::permissive(Some("https://a.com"), None, None);
        assert!(outcome.allowed);
        assert_eq!(outcome.allow_origin.as_deref(), Some("https://a.com"));
        assert_eq!(outcome.allow_headers.as_deref(), Some("*"));
    }

    #[test]
    fn an_actual_request_is_refused_by_origin_and_by_method() {
        let c = cors(&["https://a.com"], &["GET"]);
        assert_eq!(enforce(Some(&c), Some("https://evil.com"), "GET"),
                   Err(CorsRejection::Origin("https://evil.com".into())));
        assert_eq!(enforce(Some(&c), Some("https://a.com"), "DELETE"),
                   Err(CorsRejection::Method("DELETE".into())));
        assert_eq!(enforce(Some(&c), Some("https://a.com"), "GET"), Ok(()));
        assert_eq!(enforce(Some(&c), Some("https://a.com"), "get"), Ok(()),
                   "method matching is case-insensitive");
    }

    #[test]
    fn an_actual_request_without_an_origin_is_never_refused() {
        let c = cors(&["https://a.com"], &["GET"]);
        assert_eq!(enforce(Some(&c), None, "GET"), Ok(()));
        assert_eq!(enforce(None, None, "GET"), Ok(()));
    }

    #[test]
    fn an_upstream_without_a_policy_denies_every_cross_origin_request() {
        assert_eq!(
            enforce(None, Some("https://a.com"), "GET"),
            Err(CorsRejection::Origin("https://a.com".into())),
            "CORS is disabled unless explicitly enabled"
        );
        let mut disabled = cors(&["*"], &["GET"]);
        disabled.enabled = false;
        assert_eq!(
            enforce(Some(&disabled), Some("https://a.com"), "GET"),
            Err(CorsRejection::Origin("https://a.com".into()))
        );
    }

    #[test]
    fn an_allowed_actual_request_carries_the_cors_headers() {
        let mut c = cors(&["https://a.com"], &["GET"]);
        c.expose_headers = vec!["X-Request-ID".into()];
        c.allow_credentials = true;
        let headers = response_headers(Some(&c), "https://a.com");
        assert!(headers.contains(&("access-control-allow-origin".into(), "https://a.com".into())));
        assert!(headers.contains(&("access-control-expose-headers".into(), "X-Request-ID".into())));
        assert!(headers.contains(&("access-control-allow-credentials".into(), "true".into())));
        // A policy that exposes nothing advertises nothing.
        let bare = response_headers(Some(&cors(&["*"], &["GET"])), "https://a.com");
        assert_eq!(bare, vec![("access-control-allow-origin".into(), "https://a.com".into())]);
    }
}
