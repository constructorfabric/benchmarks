// Created: 2026-08-31 by Constructor Tech
//! The built-in CORS handler of the proxy data plane (ADR-0004).
//!
//! # Preflight
//!
//! A preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered locally with a permissive 204: no upstream resolution, no plugin
//! chain, no rate limit. Browser preflights carry no credentials, so no tenant
//! context exists to resolve an upstream with; origin and method validation is
//! therefore **deferred to the actual request**, which is the ADR's design and
//! not a shortcut (ADR-0004 "Preflight Request Handling"). An `OPTIONS` that is
//! not a preflight is not a CORS request at all and keeps flowing through the
//! normal proxy path.
//!
//! The cost of that deference is that a preflight is unmetered: it consumes no
//! token and no plugin run, so a flood of preflights is a request volume this
//! gear never charges for. ADR-0004 delegates that to the edge controls of the
//! deployment, which this gear does not carry — the residual risk is recorded
//! as a deviation of the design, not closed here.
//!
//! # Actual request
//!
//! Enforcement happens after the upstream and the route have been resolved and
//! before the dial, so a disallowed origin never costs an upstream call — and,
//! because CORS runs before the rate limit, never a token either. The `enabled`
//! switch of the selected record gates the whole check: CORS is off unless a
//! record turns it on, and a disabled record adds no header of its own
//! (ADR-0004 "Security Considerations", deny by default).
//!
//! An **enabled** policy is authoritative: the `access-control-*` headers the
//! upstream answered with are stripped before the gateway adds its own, so a
//! policy the operator configured cannot be widened by an upstream. A
//! **disabled** one leaves the upstream's headers exactly as they arrived,
//! because there is nothing to be authoritative about.
//!
//! # Origin matching
//!
//! Exact string comparison: no patterns, no suffix matching, port-sensitive
//! and protocol-sensitive (ADR-0004 "Security Considerations"). `*` matches
//! every origin and nothing else does — so `https://evil.com.example.com` is
//! not `https://example.com`.
//!
//! # Hierarchical configuration
//!
//! Levels are read descendant→ancestor — `[route, resolved_upstream, nearest
//! ancestor, …]`, each `None` when that record declares no `cors` member — and
//! the first level that declares one is the *selected* one. No level declares →
//! no enforcement and no header. Everything but the origin set comes from the
//! selected level; the origin set follows the sharing modes of the chain:
//!
//! * `private` (the default) — the selected level's origins alone.
//! * `inherit` — the selected level's origins unioned with **every** ancestor
//!   that declares a config.
//! * `enforce` — the origin set of the **nearest** ancestor that declares a
//!   config, whatever its size: an empty set is a deliberate deny-all and is
//!   honoured, never skipped. The selected level's own origins are dropped,
//!   because the child cannot widen what the ancestor enforces.

use http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
};
use http::{HeaderMap, HeaderValue, Method};

use axum::response::IntoResponse;

use crate::domain::model::{CorsConfig, SharingMode};
use crate::error::{OagwError, OagwErrorKind, OagwResult};

/// `Access-Control-Max-Age` of a preflight answer, in seconds (ADR-0004
/// "Preflight Request Handling").
const PREFLIGHT_MAX_AGE: &str = "86400";
/// `Vary` of a preflight answer: all three request headers it echoed.
const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";
/// `Vary` of an actual cross-origin response: the origin set is per request.
const ALLOW_VARY: &str = "Origin";
/// The wildcard origin.
const WILDCARD: &str = "*";
/// The methods the configuration schema defaults to.
const DEFAULT_METHODS: [&str; 2] = ["GET", "POST"];

/// The effective CORS policy of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// Origins that may read the response; `["*"]` allows every origin.
    pub allowed_origins: Vec<String>,
    /// Methods the upstream accepts on a cross-origin request.
    pub allowed_methods: Vec<String>,
    /// Headers to expose to the browser beyond the safelisted ones.
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are allowed.
    pub allow_credentials: bool,
}

impl Effective {
    /// Whether `origin` is allowed, by exact comparison or by the wildcard.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == WILDCARD || allowed == origin)
    }

    /// Whether `method` is allowed, ignoring the case of the spelling.
    #[must_use]
    pub fn allows_method(&self, method: &Method) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
    }

    /// The `Access-Control-Allow-Origin` value of a cross-origin response.
    ///
    /// The request's origin is echoed back, because a literal `*` is unusable
    /// for a credentialed response (ADR-0004 "Cannot use `allow_credentials`
    /// with wildcard origin"); the wildcard is only ever sent back when the
    /// policy allows no credentials.
    fn allow_origin<'origin>(&self, origin: &'origin str) -> &'origin str {
        if self.wildcard() && !self.allow_credentials {
            WILDCARD
        } else {
            origin
        }
    }

    /// Whether the origin set is the wildcard one.
    fn wildcard(&self) -> bool {
        self.allowed_origins.iter().any(|origin| origin == WILDCARD)
    }
}

/// Whether the request is a CORS preflight (ADR-0004 "Preflight Request
/// Handling").
///
/// All three conditions are required, so a plain `OPTIONS` that a route might
/// still proxy is left alone.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(ORIGIN)
        && headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

/// Answer a preflight without resolving anything.
///
/// The permissive echo is the ADR's design: the browser reads only whether its
/// request would be possible, and the actual request is where the origin and
/// the method are checked. Nothing is dialled, so a preflight of an unknown
/// alias still succeeds — the follow-up request carries the tenant context and
/// gets the real answer.
#[must_use]
pub fn preflight(headers: &HeaderMap) -> axum::response::Response {
    let mut cors = HeaderMap::new();
    copy(headers.get(ORIGIN), &mut cors, ACCESS_CONTROL_ALLOW_ORIGIN);
    copy(
        headers.get(ACCESS_CONTROL_REQUEST_METHOD),
        &mut cors,
        ACCESS_CONTROL_ALLOW_METHODS,
    );
    // Verbatim, only when the browser asked for headers (ADR-0004).
    if let Some(requested) = headers.get(ACCESS_CONTROL_REQUEST_HEADERS) {
        copy(Some(requested), &mut cors, ACCESS_CONTROL_ALLOW_HEADERS);
    }
    cors.insert(
        ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    cors.insert(VARY, HeaderValue::from_static(PREFLIGHT_VARY));
    (http::StatusCode::NO_CONTENT, cors).into_response()
}

/// Echo one request header into the answer under its CORS name.
fn copy(value: Option<&HeaderValue>, cors: &mut HeaderMap, name: http::HeaderName) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        cors.insert(name, value.clone());
    }
}

/// The effective policy of one request, or `None` when nothing enforces one.
///
/// `levels` are the `cors` members of the chain, descendant→ancestor, each
/// `None` when that record declares none: the route's, the resolved upstream's,
/// then the ancestors nearest first. `None` values and members with an empty
/// origin list are different things, which is why the levels are handed over as
/// options — the first is "this record says nothing about CORS", the second is
/// "this record deliberately allows nothing".
///
/// Only the origin set is shared, and only for a record that asks for it: see
/// the module docs for the three sharing modes.
#[must_use]
pub fn effective(levels: &[Option<&CorsConfig>]) -> Option<Effective> {
    let selected = *levels.iter().flatten().next()?;
    if !selected.enabled {
        return None;
    }
    Some(Effective {
        allowed_origins: origins(selected, levels),
        allowed_methods: methods(selected),
        expose_headers: selected.expose_headers.clone(),
        allow_credentials: selected.allow_credentials,
    })
}

/// Effective origin set of the selected config, given the whole level list.
///
/// `levels[0]` is the selected level; the rest are its ancestors, nearest
/// first, `None` where a record declares no `cors` member.
fn origins(selected: &CorsConfig, levels: &[Option<&CorsConfig>]) -> Vec<String> {
    let mut ancestors = levels.iter().flatten().skip(1);
    match selected.sharing {
        SharingMode::Private => selected.allowed_origins.clone(),
        // Every ancestor that declares a config widens the set; one that says
        // nothing about CORS contributes nothing.
        SharingMode::Inherit => {
            let mut origins = selected.allowed_origins.clone();
            for ancestor in ancestors {
                origins.extend(ancestor.allowed_origins.iter().cloned());
            }
            origins
        }
        // The nearest ancestor that declares a config wins, whatever its size:
        // an empty list is a deny-all the child cannot talk its way out of.
        // Without an ancestor there is nothing to enforce, so the selected
        // level's own origins stand.
        SharingMode::Enforce => ancestors.next().map_or_else(
            || selected.allowed_origins.clone(),
            |ancestor| ancestor.allowed_origins.clone(),
        ),
    }
}

/// Methods of `config`; the schema default when the record leaves them out.
fn methods(config: &CorsConfig) -> Vec<String> {
    if config.allowed_methods.is_empty() {
        DEFAULT_METHODS.iter().map(ToString::to_string).collect()
    } else {
        config.allowed_methods.clone()
    }
}

/// Enforce the effective policy on one actual request (ADR-0004 "Error
/// Responses").
///
/// A request without an `Origin` is not a CORS request and always passes.
///
/// # Errors
/// 403 for an origin that is not in the effective set, and 403 for a method
/// the policy does not allow.
pub fn check(effective: &Effective, method: &Method, headers: &HeaderMap) -> OagwResult<()> {
    let Some(origin) = headers.get(ORIGIN).and_then(|origin| origin.to_str().ok()) else {
        return Ok(());
    };
    if origin.is_empty() {
        return Ok(());
    }
    if !effective.allows_origin(origin) {
        return Err(OagwError::new(
            OagwErrorKind::CorsOriginNotAllowed,
            format!("origin '{origin}' is not in the allowed origins list"),
        ));
    }
    if !effective.allows_method(method) {
        return Err(OagwError::new(
            OagwErrorKind::CorsMethodNotAllowed,
            format!("method '{method}' is not in the allowed methods list"),
        ));
    }
    Ok(())
}

/// Headers a forwarded cross-origin response carries (ADR-0004 "Response
/// Headers").
///
/// `origin` is the `Origin` of the request: it is echoed back because a
/// credentialed answer cannot say `*`. A same-origin request, which has no
/// `Origin`, gets no CORS header at all but still the `Vary`, because the
/// answer depends on the origin whether or not it names one.
#[must_use]
pub fn response_headers(effective: &Effective, origin: Option<&str>) -> HeaderMap {
    let mut cors = HeaderMap::new();
    if let Some(origin) = origin.filter(|origin| !origin.is_empty()) {
        insert(
            &mut cors,
            ACCESS_CONTROL_ALLOW_ORIGIN,
            effective.allow_origin(origin),
        );
    }
    if !effective.expose_headers.is_empty() {
        insert(
            &mut cors,
            ACCESS_CONTROL_EXPOSE_HEADERS,
            &effective.expose_headers.join(", "),
        );
    }
    if effective.allow_credentials {
        cors.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    cors.insert(VARY, HeaderValue::from_static(ALLOW_VARY));
    cors
}

/// Headers a refused request is answered with (ADR-0004 "Error Responses").
///
/// No `Access-Control-Allow-Origin`: the origin was not allowed, and naming it
/// would tell the browser the opposite of what the answer says. The `Vary`
/// stays, because the answer still depends on the origin the request named.
#[must_use]
pub fn denied_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(VARY, HeaderValue::from_static(ALLOW_VARY));
    headers
}

/// Strip the `access-control-*` headers an upstream answered with.
///
/// An enabled policy is authoritative for its origins, so the upstream's own
/// CORS answer is dropped before the gateway adds the one its configuration
/// asks for. Headers the gateway does not own are left alone.
pub fn strip_upstream_headers(headers: &mut HeaderMap) {
    let owned: Vec<http::HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with(UPSTREAM_PREFIX))
        .cloned()
        .collect();
    for name in owned {
        headers.remove(&name);
    }
}

/// The prefix every CORS response header shares.
const UPSTREAM_PREFIX: &str = "access-control-";

/// Insert a header value from a `&str`, skipping one the wire cannot carry.
///
/// An origin the caller controls can still be malformed, and a header value
/// that fails to parse is dropped rather than propagated.
fn insert(cors: &mut HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        cors.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue};

    use super::{
        Effective, check, denied_headers, effective, is_preflight, preflight, response_headers,
        strip_upstream_headers,
    };
    use crate::domain::model::{CorsConfig, SharingMode};

    fn config(sharing: SharingMode, origins: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing,
            enabled: true,
            allowed_origins: origins.iter().map(ToString::to_string).collect(),
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    fn effective_of(config: &CorsConfig) -> Effective {
        effective(&[Some(config)]).unwrap_or_else(|| panic!("an enabled config resolves"))
    }

    fn headers(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_str(origin)
                .unwrap_or_else(|_| HeaderValue::from_static("about:blank")),
        );
        headers
    }

    #[test]
    fn only_options_with_a_request_method_is_a_preflight() {
        let headers = preflight_headers();
        assert!(is_preflight(&http::Method::OPTIONS, &headers));
        assert!(!is_preflight(&http::Method::GET, &headers));
        // A plain `OPTIONS` without the CORS headers is not a preflight.
        assert!(!is_preflight(&http::Method::OPTIONS, &HeaderMap::new()));
        let mut without_origin = headers;
        without_origin.remove(http::header::ORIGIN);
        assert!(!is_preflight(&http::Method::OPTIONS, &without_origin));
    }

    fn preflight_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://browser.example.com"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("PUT"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_HEADERS,
            HeaderValue::from_static("x-trace, content-type"),
        );
        headers
    }

    #[test]
    fn a_preflight_is_answered_with_the_permissive_echo() {
        let response = preflight(&preflight_headers());
        assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
        let cors = response.headers();
        assert_eq!(
            cors.get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("https://browser.example.com")
        );
        assert_eq!(
            cors.get(http::header::ACCESS_CONTROL_ALLOW_METHODS)
                .and_then(|value| value.to_str().ok()),
            Some("PUT")
        );
        assert_eq!(
            cors.get(http::header::ACCESS_CONTROL_ALLOW_HEADERS)
                .and_then(|value| value.to_str().ok()),
            Some("x-trace, content-type")
        );
        assert_eq!(
            cors.get(http::header::ACCESS_CONTROL_MAX_AGE)
                .and_then(|value| value.to_str().ok()),
            Some("86400")
        );
        assert_eq!(
            cors.get(http::header::VARY)
                .and_then(|value| value.to_str().ok()),
            Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
        );
    }

    #[test]
    fn a_preflight_without_requested_headers_names_none() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://a.example.com"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("GET"),
        );
        let response = preflight(&headers);
        assert!(
            !response
                .headers()
                .contains_key(http::header::ACCESS_CONTROL_ALLOW_HEADERS)
        );
    }

    #[test]
    fn a_disabled_config_enforces_nothing() {
        let mut config = config(
            crate::domain::model::SharingMode::Private,
            &["https://a.example.com"],
        );
        config.enabled = false;
        assert!(effective(&[Some(&config)]).is_none());
    }

    #[test]
    fn a_private_config_ignores_its_ancestors() {
        let own = config(
            crate::domain::model::SharingMode::Private,
            &["https://child.example.com"],
        );
        let parent = config(
            crate::domain::model::SharingMode::Private,
            &["https://parent.example.com"],
        );
        let cors = effective(&[Some(&own), Some(&parent)]).unwrap_or_else(|| panic!("resolves"));
        assert_eq!(cors.allowed_origins, ["https://child.example.com"]);
    }

    #[test]
    fn an_inherited_config_unions_the_origins() {
        let own = config(
            crate::domain::model::SharingMode::Inherit,
            &["https://child.example.com"],
        );
        let parent = config(
            crate::domain::model::SharingMode::Inherit,
            &["https://parent.example.com"],
        );
        let grandparent = config(
            crate::domain::model::SharingMode::Inherit,
            &["https://root.example.com"],
        );
        let cors = effective(&[Some(&own), Some(&parent), Some(&grandparent)])
            .unwrap_or_else(|| panic!("resolves"));
        assert_eq!(
            cors.allowed_origins,
            [
                "https://child.example.com",
                "https://parent.example.com",
                "https://root.example.com"
            ]
        );
    }

    #[test]
    fn an_enforced_config_keeps_the_parent_origins() {
        let own = config(
            crate::domain::model::SharingMode::Enforce,
            &["https://child.example.com"],
        );
        let parent = config(
            crate::domain::model::SharingMode::Enforce,
            &["https://parent.example.com"],
        );
        let cors = effective(&[Some(&own), Some(&parent)]).unwrap_or_else(|| panic!("resolves"));
        assert_eq!(cors.allowed_origins, ["https://parent.example.com"]);
    }

    #[test]
    fn an_enforce_ancestor_with_no_origins_denies_everything() {
        let own = config(
            crate::domain::model::SharingMode::Enforce,
            &["https://child.example.com"],
        );
        let parent = config(crate::domain::model::SharingMode::Enforce, &[]);
        let cors = effective(&[Some(&own), Some(&parent)]).unwrap_or_else(|| panic!("resolves"));
        // An empty set is a deliberate deny-all, not a missing configuration.
        assert!(
            cors.allowed_origins.is_empty(),
            "the ancestor's deny-all must survive: {:?}",
            cors.allowed_origins
        );
        assert!(
            check(
                &cors,
                &http::Method::GET,
                &headers("https://child.example.com")
            )
            .is_err()
        );
    }

    #[test]
    fn an_enforced_ancestor_set_survives_a_route_policy_of_its_own() {
        // The route declares `enforce`, so the upstream's origin set is what
        // the request is judged against, however the route spells its own.
        let own = config(
            crate::domain::model::SharingMode::Enforce,
            &["https://route.example.com"],
        );
        let parent = config(
            crate::domain::model::SharingMode::Private,
            &["https://parent.example.com"],
        );
        let grandparent = config(
            crate::domain::model::SharingMode::Enforce,
            &["https://root.example.com"],
        );
        let cors = effective(&[Some(&own), Some(&parent), Some(&grandparent)])
            .unwrap_or_else(|| panic!("resolves"));
        assert_eq!(cors.allowed_origins, ["https://parent.example.com"]);
    }

    #[test]
    fn an_enforced_ancestor_without_a_config_of_its_own_is_ignored() {
        let own = config(
            crate::domain::model::SharingMode::Enforce,
            &["https://child.example.com"],
        );
        // The ancestor declares no `cors` member at all, so there is nothing to
        // enforce and the selected level's origins stand.
        let cors = effective(&[Some(&own), None]).unwrap_or_else(|| panic!("resolves"));
        assert_eq!(cors.allowed_origins, ["https://child.example.com"]);
    }

    #[test]
    fn a_refusal_names_no_origin_at_all() {
        let denied = denied_headers();
        assert!(denied.contains_key(http::header::VARY));
        assert!(!denied.contains_key(http::header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[test]
    fn an_upstreams_own_cors_answer_is_stripped() {
        let mut upstream = HeaderMap::new();
        upstream.insert(
            http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        upstream.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        upstream.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        strip_upstream_headers(&mut upstream);
        assert!(!upstream.contains_key(http::header::ACCESS_CONTROL_ALLOW_ORIGIN));
        assert!(!upstream.contains_key(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS));
        assert_eq!(
            upstream
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain"),
            "headers the gateway does not own are left alone"
        );
    }

    #[test]
    fn a_method_list_defaults_to_the_schema() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &["*"]));
        assert_eq!(cors.allowed_methods, ["GET", "POST"]);
        let mut declared = config(crate::domain::model::SharingMode::Private, &["*"]);
        declared.allowed_methods = vec!["PUT".to_owned(), "delete".to_owned()];
        let cors = effective(&[Some(&declared)]).unwrap_or_else(|| panic!("resolves"));
        assert_eq!(cors.allowed_methods, ["PUT", "delete"]);
    }

    #[test]
    fn an_origin_is_matched_exactly() {
        let cors = effective_of(&config(
            crate::domain::model::SharingMode::Private,
            &["https://example.com"],
        ));
        assert!(cors.allows_origin("https://example.com"));
        assert!(check(&cors, &http::Method::GET, &headers("https://example.com")).is_ok());
        // No suffix matching: the attacker's host only shares a suffix.
        assert!(
            check(
                &cors,
                &http::Method::GET,
                &headers("https://evil.com.example.com")
            )
            .is_err()
        );
        // Port- and protocol-sensitive.
        assert!(
            check(
                &cors,
                &http::Method::GET,
                &headers("https://example.com:8443")
            )
            .is_err()
        );
        assert!(check(&cors, &http::Method::GET, &headers("http://example.com")).is_err());
    }

    #[test]
    fn a_wildcard_matches_every_origin() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &["*"]));
        assert!(cors.allows_origin("https://anything.example.com"));
        assert!(cors.allows_origin("http://localhost:3000"));
    }

    #[test]
    fn a_method_check_ignores_the_case_of_the_spelling() {
        let mut config = config(crate::domain::model::SharingMode::Private, &["*"]);
        config.allowed_methods = vec!["POST".to_owned()];
        let cors = effective(&[Some(&config)]).unwrap_or_else(|| panic!("resolves"));
        assert!(
            check(
                &cors,
                &http::Method::POST,
                &headers("https://a.example.com")
            )
            .is_ok()
        );
        // The verb the browser spelled is matched, not the case it used.
        assert!(
            check(
                &cors,
                &http::Method::from_bytes(b"post").unwrap_or(http::Method::POST),
                &headers("https://a.example.com")
            )
            .is_ok()
        );
        assert!(
            check(
                &cors,
                &http::Method::DELETE,
                &headers("https://a.example.com")
            )
            .is_err()
        );
    }

    #[test]
    fn a_method_check_falls_back_to_the_schema_default() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &["*"]));
        assert!(
            check(
                &cors,
                &http::Method::POST,
                &headers("https://a.example.com")
            )
            .is_ok()
        );
        assert!(
            check(
                &cors,
                &http::Method::DELETE,
                &headers("https://a.example.com")
            )
            .is_err()
        );
    }

    #[test]
    fn a_request_without_an_origin_is_not_a_cors_request() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &["*"]));
        assert!(check(&cors, &http::Method::GET, &HeaderMap::new()).is_ok());
    }

    #[test]
    fn a_forwarded_response_echoes_the_request_origin() {
        let mut config = config(
            crate::domain::model::SharingMode::Private,
            &["https://a.example.com"],
        );
        config.expose_headers = vec!["x-request-id".to_owned()];
        config.allow_credentials = true;
        let cors = effective(&[Some(&config)]).unwrap_or_else(|| panic!("resolves"));
        let headers = response_headers(&cors, Some("https://a.example.com"));
        assert_eq!(
            headers
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("https://a.example.com"),
            "a credentialed answer echoes the origin instead of `*`"
        );
        assert_eq!(
            headers
                .get(http::header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .and_then(|value| value.to_str().ok()),
            Some("x-request-id")
        );
        assert_eq!(
            headers
                .get(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            headers
                .get(http::header::VARY)
                .and_then(|value| value.to_str().ok()),
            Some("Origin")
        );
    }

    #[test]
    fn a_wildcard_policy_without_credentials_answers_the_wildcard() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &["*"]));
        let headers = response_headers(&cors, Some("https://a.example.com"));
        assert_eq!(
            headers
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
        assert!(!headers.contains_key(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS));
    }

    #[test]
    fn a_same_origin_response_still_varies_on_the_origin() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &["*"]));
        let headers = response_headers(&cors, None);
        assert!(!headers.contains_key(http::header::ACCESS_CONTROL_ALLOW_ORIGIN));
        assert_eq!(
            headers
                .get(http::header::VARY)
                .and_then(|value| value.to_str().ok()),
            Some("Origin")
        );
    }

    #[test]
    fn a_policy_of_no_origins_allows_nothing() {
        let cors = effective_of(&config(crate::domain::model::SharingMode::Private, &[]));
        assert!(check(&cors, &http::Method::GET, &headers("https://a.example.com")).is_err());
    }
}
