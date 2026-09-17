//! CORS handling (feature `cpt-cf-oagw-feature-cors-handling`, ADR 0004).
//!
//! Core Data Plane logic, *not* a `GuardPlugin` implementation (FEATURE §49):
//! only the `cors` *catalog identifier* exists under Plugin System.  This
//! module provides:
//!
//! - **Preflight detection** (algorithm `cpt-cf-oagw-algo-cors-handling-detect-preflight`,
//!   step `inst-co-detect-check`): `OPTIONS` + `Origin` + `Access-Control-Request-Method`;
//! - **Permissive preflight fast path** (DoD `cpt-cf-oagw-dod-cors-handling-preflight`,
//!   flow `cpt-cf-oagw-flow-cors-handling-preflight`, step `inst-co-preflight-204`):
//!   204 echoing the requested origin/method/headers with
//!   `Access-Control-Max-Age: 86400` and
//!   `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`,
//!   handled without tenant resolution or an upstream round-trip (step
//!   `inst-co-preflight-bypass` is enforced by the request handler);
//! - **Actual-request enforcement** (DoD `cpt-cf-oagw-dod-cors-handling-actual`,
//!   flow `cpt-cf-oagw-flow-cors-handling-actual`): exact-match origin
//!   validation (protocol- and port-sensitive, `*` matches any, no regex —
//!   steps `inst-co-origin-wildcard`/`inst-co-origin-exact`), method
//!   validation (step `inst-co-method-ok`), 403 rejections with `Vary: Origin`
//!   (`cors.origin_not_allowed` / `cors.method_not_allowed`), and the CORS
//!   response headers on allowed responses (step `inst-co-actual-ok`).
//!
//! The hierarchical union/force merge lives in [`crate::domain::merge`]
//! (algorithm `cpt-cf-oagw-algo-cors-handling-merge-hierarchy`, steps
//! `inst-co-merge-inherit`/`inst-co-merge-enforce`), and credential-wildcard
//! rejection at configuration validation time (step `inst-co-merge-creds`) is
//! surfaced here via [`validate_config`].
//!
//! The DESIGN error table (§3.3) carries the two CORS rejection instances
//! (`cf.oagw.cors.origin_not_allowed.v1` / `cf.oagw.cors.method_not_allowed.v1`,
//! 403); [`CorsViolation`] carries the full GTS instance and bridges to the
//! matching [`DomainError`] variant for the typed-error and audit paths.

use crate::domain::entity::config::CorsConfig;
use crate::domain::error::DomainError;
use crate::domain::plugin::Headers;

/// `Access-Control-Max-Age` on the permissive preflight (ADR 0004).
pub const PREFLIGHT_MAX_AGE_SECS: u64 = 86400;
/// The preflight `Vary` value (ADR 0004).
pub const VARY_PREFLIGHT: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";
/// The `Vary` value on actual-request responses (ADR 0004: always include
/// `Vary: Origin` to prevent cache poisoning).
pub const VARY_ACTUAL: &str = "Origin";

/// Classifies a request as a CORS preflight (algorithm
/// `cpt-cf-oagw-algo-cors-handling-detect-preflight`): method `OPTIONS` with
/// both `Origin` and `Access-Control-Request-Method` present (step
/// `inst-co-detect-check`).
#[must_use]
pub fn is_preflight(method: &str, headers: &Headers) -> bool {
    method.eq_ignore_ascii_case("OPTIONS")
        && headers.contains("origin")
        && headers.contains("access-control-request-method")
}

/// The response-disposition of a CORS preflight.
///
/// Preflights are permissive by design (flow
/// `cpt-cf-oagw-flow-cors-handling-preflight`): the gateway echoes whatever
/// the browser requested.  The value carries the exact header pairs the
/// handler sets on the 204 — no upstream round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightResponse {
    /// `(header_name, value)` pairs: `Access-Control-Allow-Origin`,
    /// `Access-Control-Allow-Methods`, `Access-Control-Max-Age`,
    /// `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`
    /// and, when the request sent `Access-Control-Request-Headers`, that
    /// echoed back.
    pub headers: Vec<(String, String)>,
}

/// Computes the permissive 204 preflight response (step `inst-co-preflight-204`).
///
/// `origin` / `request_method` / `request_headers` are the verbatim request
/// header values.  `request_headers` ('Access-Control-Request-Headers') is
/// echoed only when present.
#[must_use]
pub fn preflight_response(
    origin: &str,
    request_method: &str,
    request_headers: Option<&str>,
) -> PreflightResponse {
    let mut headers = Vec::with_capacity(5);
    headers.push(("Access-Control-Allow-Origin".to_owned(), origin.to_owned()));
    headers.push((
        "Access-Control-Allow-Methods".to_owned(),
        request_method.to_owned(),
    ));
    if let Some(acrh) = request_headers
        && !acrh.trim().is_empty()
    {
        headers.push(("Access-Control-Allow-Headers".to_owned(), acrh.to_owned()));
    }
    headers.push((
        "Access-Control-Max-Age".to_owned(),
        PREFLIGHT_MAX_AGE_SECS.to_string(),
    ));
    headers.push(("Vary".to_owned(), VARY_PREFLIGHT.to_owned()));
    PreflightResponse { headers }
}

/// Disposition of an actual cross-origin request (flow
/// `cpt-cf-oagw-flow-cors-handling-actual`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsOutcome {
    /// CORS is disabled on the effective config, or the request carried no
    /// `Origin` — no CORS validation applies and no CORS headers are added
    /// (step `inst-co-actual-disabled`).
    NotEnabled,
    /// Origin and method allowed — the request proceeds and the handler adds
    /// the CORS response headers (step `inst-co-actual-ok`).
    Allowed(CorsHeaders),
    /// Origin or method not allowed — reject 403 with `Vary: Origin` (steps
    /// `inst-co-actual-origin`/`inst-co-actual-method`).
    Violation(CorsViolation),
}

/// The CORS response headers for an allowed actual request (step
/// `inst-co-actual-ok`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsHeaders {
    /// `Access-Control-Allow-Origin` — the echoed request origin, or `*` for
    /// the wildcard configuration.
    pub allow_origin: String,
    /// Whether `Access-Control-Allow-Credentials: true` is emitted (never
    /// with the wildcard origin — defensive, config validation also rejects
    /// the combination at write time, step `inst-co-merge-creds`).
    pub allow_credentials: bool,
    /// `Access-Control-Expose-Headers` values (only emitted when non-empty).
    pub expose_headers: Vec<String>,
}

impl CorsHeaders {
    /// Renders the headers to apply to the allowed response — including
    /// `Vary: Origin` (ADR 0004: always include it to prevent cache
    /// poisoning).
    #[must_use]
    pub fn as_header_pairs(&self) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(4);
        out.push((
            "Access-Control-Allow-Origin".to_owned(),
            self.allow_origin.clone(),
        ));
        if self.allow_credentials {
            out.push((
                "Access-Control-Allow-Credentials".to_owned(),
                "true".to_owned(),
            ));
        }
        if !self.expose_headers.is_empty() {
            out.push((
                "Access-Control-Expose-Headers".to_owned(),
                self.expose_headers.join(", "),
            ));
        }
        out.push(("Vary".to_owned(), VARY_ACTUAL.to_owned()));
        out
    }
}

/// A CORS rejection (ADR 0004 / DESIGN error table `cors.*` GTS instances).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsViolation {
    /// The GTS error instance: `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`
    /// or `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`.
    pub code: &'static str,
    /// The rejection status — always 403 (ADR 0004).
    pub status: u16,
    /// Human-readable detail.
    pub detail: String,
}

/// The `cf.oagw.cors.origin_not_allowed` GTS error instance (403).
pub const ORIGIN_NOT_ALLOWED: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// The `cf.oagw.cors.method_not_allowed` GTS error instance (403).
pub const METHOD_NOT_ALLOWED: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
/// Rejection status for CORS violations (ADR 0004).
pub const CORS_REJECT_STATUS: u16 = 403;

impl CorsViolation {
    /// Constructs an origin rejection (`inst-co-origin-reject`).
    #[must_use]
    pub fn origin(origin: &str) -> Self {
        Self {
            code: ORIGIN_NOT_ALLOWED,
            status: CORS_REJECT_STATUS,
            detail: format!("origin '{origin}' is not in the allowed origins"),
        }
    }

    /// Constructs a method rejection (`inst-co-method-reject`).
    #[must_use]
    pub fn method(method: &str) -> Self {
        Self {
            code: METHOD_NOT_ALLOWED,
            status: CORS_REJECT_STATUS,
            detail: format!("method '{method}' is not in the allowed methods"),
        }
    }

    /// Bridges the violation to the matching DESIGN error instance
    /// (`cf.oagw.cors.*`, 403).
    #[must_use]
    pub fn to_domain_error(&self) -> DomainError {
        match self.code {
            ORIGIN_NOT_ALLOWED => DomainError::CorsOriginNotAllowed {
                detail: self.detail.clone(),
            },
            METHOD_NOT_ALLOWED => DomainError::CorsMethodNotAllowed {
                detail: self.detail.clone(),
            },
            _ => DomainError::validation(
                None,
                format!("CORS violation ({}): {}", self.code, self.detail),
            ),
        }
    }
}

/// Evaluates an actual request against the effective [`CorsConfig`]
/// (algorithm `cpt-cf-oagw-algo-cors-handling-validate-origin` /
/// `cpt-cf-oagw-algo-cors-handling-validate-method`).
///
/// `origin` is the verbatim `Origin` header value; requests without an
/// `Origin` are not cross-origin and receive no CORS processing
/// (`inst-co-actual-disabled`).  Origin matching is exact string comparison —
/// protocol- and port-sensitive, `*` matches any origin, and no regex is
/// supported (`https://evil.com.example.com` does *not* match
/// `https://example.com`).  Method matching is case-insensitive.
#[must_use]
pub fn evaluate_actual(method: &str, origin: Option<&str>, config: &CorsConfig) -> CorsOutcome {
    // Disabled unless explicitly enabled; also skip when not a cross-origin
    // request (no Origin header) (inst-co-actual-disabled).
    if !config.enabled {
        return CorsOutcome::NotEnabled;
    }
    let Some(origin) = origin else {
        return CorsOutcome::NotEnabled;
    };

    // Origin exact match (inst-co-origin-wildcard / inst-co-origin-exact).
    let wildcard = config.allowed_origins.iter().any(|o| o == "*");
    let origin_allowed = wildcard || config.allowed_origins.iter().any(|o| o == origin);
    if !origin_allowed {
        return CorsOutcome::Violation(CorsViolation::origin(origin));
    }

    // Method validation (inst-co-method-ok / inst-co-method-reject).
    let method_allowed = config
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method));
    if !method_allowed {
        return CorsOutcome::Violation(CorsViolation::method(method));
    }

    // Credentials combined with the wildcard origin is rejected at config
    // validation time (inst-co-merge-creds); defensively never emit
    // `Allow-Credentials: true` with `*` here either.
    let allow_credentials = config.allow_credentials && !wildcard;
    CorsOutcome::Allowed(CorsHeaders {
        allow_origin: if wildcard {
            "*".to_owned()
        } else {
            origin.to_owned()
        },
        allow_credentials,
        expose_headers: config.expose_headers.clone(),
    })
}

/// Validates a [`CorsConfig`] at configuration time (step
/// `inst-co-merge-creds`): `allow_credentials: true` combined with the
/// wildcard origin `*` is rejected, and cannot be emitted safely by any real
/// browser (DoD `cpt-cf-oagw-dod-cors-handling-config`).
///
/// # Errors
/// - `request validation failed` (400) when credentials are combined with `*`.
pub fn validate_config(config: &CorsConfig) -> Result<(), DomainError> {
    if config.enabled && config.allow_credentials && config.allowed_origins.iter().any(|o| o == "*")
    {
        return Err(DomainError::validation(
            Some("cors"),
            "allow_credentials cannot be combined with the wildcard origin '*'",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn config(origins: &[&str], methods: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: crate::domain::entity::config::SharingMode::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
            allowed_methods: methods.iter().map(|s| s.to_string()).collect(),
            expose_headers: vec!["X-Trace".to_owned()],
            allow_credentials: credentials,
        }
    }

    #[test]
    fn preflight_detection_requires_options_origin_and_acrm() {
        let h = headers(&[
            ("Origin", "https://app.example.com"),
            ("Access-Control-Request-Method", "POST"),
        ]);
        assert!(is_preflight("OPTIONS", &h));
        assert!(is_preflight("options", &h), "case-insensitive method");
        assert!(is_preflight("options", &h), "method case-insensitive");

        // Missing any ingredient → not a preflight.
        assert!(!is_preflight("GET", &h), "not OPTIONS");
        let no_acrm = headers(&[("Origin", "https://app.example.com")]);
        assert!(!is_preflight("OPTIONS", &no_acrm), "no ACRM");
        let no_origin = headers(&[("Access-Control-Request-Method", "POST")]);
        assert!(!is_preflight("OPTIONS", &no_origin), "no Origin");
    }

    #[test]
    fn preflight_204_echoes_request_and_sets_max_age_and_vary() {
        let p = preflight_response(
            "https://app.example.com",
            "POST",
            Some("X-Custom, Content-Type"),
        );
        let values: Vec<(String, String)> = p.headers.clone();
        assert!(values.contains(&(
            "Access-Control-Allow-Origin".to_owned(),
            "https://app.example.com".to_owned()
        )));
        assert!(values.contains(&("Access-Control-Allow-Methods".to_owned(), "POST".to_owned())));
        assert!(values.contains(&(
            "Access-Control-Allow-Headers".to_owned(),
            "X-Custom, Content-Type".to_owned()
        )));
        assert!(values.contains(&("Access-Control-Max-Age".to_owned(), "86400".to_owned())));
        assert!(values.contains(&(
            "Vary".to_owned(),
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned()
        )));

        // No Access-Control-Request-Headers → not echoed.
        let bare = preflight_response("https://a.example.com", "GET", None);
        assert!(
            !bare
                .headers
                .iter()
                .any(|(k, _)| k == "Access-Control-Allow-Headers"),
            "ACRH absent → no Allow-Headers"
        );
        // Blank ACRH is treated as absent.
        let blank = preflight_response("https://a.example.com", "GET", Some("  "));
        assert!(
            !blank
                .headers
                .iter()
                .any(|(k, _)| k == "Access-Control-Allow-Headers")
        );
    }

    #[test]
    fn actual_allowed_echoes_origin_and_adds_headers() {
        let cfg = config(&["https://example.com"], &["GET", "POST"], true);
        match evaluate_actual("GET", Some("https://example.com"), &cfg) {
            CorsOutcome::Allowed(h) => {
                let pairs = h.as_header_pairs();
                assert!(pairs.contains(&(
                    "Access-Control-Allow-Origin".to_owned(),
                    "https://example.com".to_owned()
                )));
                assert!(pairs.contains(&(
                    "Access-Control-Allow-Credentials".to_owned(),
                    "true".to_owned()
                )));
                assert!(pairs.contains(&(
                    "Access-Control-Expose-Headers".to_owned(),
                    "X-Trace".to_owned()
                )));
                assert!(pairs.contains(&("Vary".to_owned(), "Origin".to_owned())));
            }
            other => panic!("expected allowed, got {other:?}"),
        }
        // Method comparison is case-insensitive.
        match evaluate_actual("post", Some("https://example.com"), &cfg) {
            CorsOutcome::Allowed(_) => {}
            other => panic!("case-insensitive method must allow, got {other:?}"),
        }
    }

    #[test]
    fn actual_origin_reject_is_403_origin_not_allowed_with_vary() {
        let cfg = config(&["https://example.com"], &["GET"], false);
        match evaluate_actual("GET", Some("https://evil.com"), &cfg) {
            CorsOutcome::Violation(v) => {
                assert_eq!(
                    v.code,
                    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
                );
                assert_eq!(v.status, 403);
                // The typed-error bridge resolves to the matching DESIGN
                // instance (403), consistent with the served raw response.
                let de = v.to_domain_error();
                assert_eq!(de.status(), 403);
                assert_eq!(
                    de.instance(),
                    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
                );
            }
            other => panic!("expected violation, got {other:?}"),
        }
    }

    #[test]
    fn actual_method_reject_is_403_method_not_allowed() {
        let cfg = config(&["https://example.com"], &["GET", "POST"], false);
        match evaluate_actual("DELETE", Some("https://example.com"), &cfg) {
            CorsOutcome::Violation(v) => {
                assert_eq!(
                    v.code,
                    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
                );
                assert_eq!(v.status, 403);
            }
            other => panic!("expected violation, got {other:?}"),
        }
    }

    #[test]
    fn origin_matching_is_exact_and_subdomain_does_not_match() {
        let cfg = config(&["https://example.com"], &["GET"], false);
        // Protocol- and port-sensitive exact match.
        for evil in [
            "https://evil.com.example.com",
            "http://example.com",
            "https://example.com.evil.com",
            "https://example.com:8443",
            "example.com",
        ] {
            match evaluate_actual("GET", Some(evil), &cfg) {
                CorsOutcome::Violation(v) => assert_eq!(v.code, ORIGIN_NOT_ALLOWED, "{evil}"),
                other => panic!("{evil} must be rejected, got {other:?}"),
            }
        }
        // Exact allow still works.
        assert!(matches!(
            evaluate_actual("GET", Some("https://example.com"), &cfg),
            CorsOutcome::Allowed(_)
        ));
    }

    #[test]
    fn wildcard_matches_any_origin_but_never_credentials() {
        let cfg = config(&["*"], &["GET"], true);
        match evaluate_actual("GET", Some("https://anything.example.net"), &cfg) {
            CorsOutcome::Allowed(h) => {
                assert_eq!(h.allow_origin, "*");
                assert!(!h.allow_credentials, "never emit credentials with '*'");
                let pairs = h.as_header_pairs();
                assert!(
                    !pairs
                        .iter()
                        .any(|(k, _)| k == "Access-Control-Allow-Credentials"),
                    "no Allow-Credentials header with wildcard"
                );
            }
            other => panic!("wildcard must allow, got {other:?}"),
        }
        // Config validation rejects the combination at write time.
        assert!(validate_config(&cfg).is_err());
        let safe = config(&["*"], &["GET"], false);
        assert!(validate_config(&safe).is_ok());
        let specific = config(&["https://example.com"], &["GET"], true);
        assert!(validate_config(&specific).is_ok());
    }

    #[test]
    fn disabled_or_missing_origin_applies_no_cors() {
        let mut cfg = config(&["https://example.com"], &["GET"], false);
        cfg.enabled = false;
        assert_eq!(
            evaluate_actual("GET", Some("https://evil.com"), &cfg),
            CorsOutcome::NotEnabled
        );
        let enabled = config(&["https://example.com"], &["GET"], false);
        assert_eq!(
            evaluate_actual("GET", None, &enabled),
            CorsOutcome::NotEnabled,
            "no Origin → no cross-origin processing"
        );
    }
}
