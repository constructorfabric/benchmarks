//! The built-in CORS handler of the proxy data plane
//! ([ADR-0004](../../../docs/ADR/0004-cors.md)).
//!
//! CORS is a first-class field of an upstream and of a route, not a plugin:
//! a preflight has to be answered before any upstream is resolved, and an
//! origin has to be rejected before the request is forwarded
//! ([DESIGN.md](../../../docs/DESIGN.md) "Cross-Origin Resource Sharing").
//! The two matrices the ADR fixes are decided here as pure functions over the
//! resolved records and the inbound headers, and
//! [`crate::api::rest::proxy`] applies them in the pipeline:
//!
//! | Request | Answer |
//! |---|---|
//! | `OPTIONS` + `Origin` + `Access-Control-Request-Method` | Permissive `204` echoing what the browser asked for. No upstream resolution, no tenant context. |
//! | actual cross-origin request, origin not allowed | `403` `~cf.oagw.cors.origin_not_allowed.v1`, `Vary: Origin`. |
//! | actual cross-origin request, method not allowed | `403` `~cf.oagw.cors.method_not_allowed.v1`, `Vary: Origin`. |
//! | actual cross-origin request, both allowed | Forwarded, and the response carries the CORS headers. |
//!
//! A preflight is answered permissively *by design* (ADR-0004: browsers send no
//! credentials on one, so there is no tenant context to resolve an upstream
//! with): the answer echoes whatever the browser asked for, and the enforcement
//! happens on the actual request that follows it. An `OPTIONS` without an
//! `Access-Control-Request-Method` is not a preflight and takes the normal
//! proxy path.
//!
//! ## Layer merge ([ADR-0004](../../../docs/ADR/0004-cors.md)
//! "Hierarchical Configuration")
//!
//! The layers are ordered ancestor → descendant (`upstream < route`) and the
//! `sharing` mode of the configuration in force decides how the next layer
//! combines, as it does for the rate limits of
//! [`crate::domain::merger`]:
//!
//! | Layer's `sharing` | Effective policy |
//! |---|---|
//! | `private` | The descendant's block when it declares one; a descendant that declares none keeps the layer's own. |
//! | `inherit` | Union of the layer's and the descendant's origins, methods and exposed headers. |
//! | `enforce` | The layer's configuration, forced: the descendant cannot add origins to it. |
//!
//! The `cors.allow_credentials` + wildcard-origin combination is rejected when
//! the configuration is built
//! ([`crate::domain::service`]), so [`EffectiveCors`] never has to re-check it.

use http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
};
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::model::{CorsConfig, Route, SharingMode, Upstream};
use crate::domain::proxy::ProxyError;

// ---------------------------------------------------------------------------
// Wire constants
// ---------------------------------------------------------------------------

/// The `Access-Control-Max-Age` of every preflight answer (ADR-0004): one day,
/// so a browser does not re-ask for every request of a session.
pub const MAX_AGE_SECS: &str = "86400";

/// The `Vary` value of a preflight answer, per the ADR-0004 example: the three
/// request headers the answer was computed from.
const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// The value `Access-Control-Allow-Credentials` takes when credentials are
/// allowed; the CORS protocol spells it out rather than using a boolean.
const ALLOW_CREDENTIALS: &str = "true";

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// `true` when a request is a CORS preflight
/// ([ADR-0004](../../../docs/ADR/0004-cors.md) "Preflight Request Handling"):
/// an `OPTIONS` carrying both an `Origin` and an
/// `Access-Control-Request-Method`.
///
/// The third header is what separates a preflight from an ordinary `OPTIONS` on
/// a proxied path: without it the request is a normal one and goes through the
/// whole pipeline, route matching included.
#[must_use]
pub fn is_preflight(method: &str, headers: &HeaderMap) -> bool {
    method.eq_ignore_ascii_case("OPTIONS")
        && header(headers, ORIGIN).is_some()
        && header(headers, ACCESS_CONTROL_REQUEST_METHOD).is_some()
}

/// The headers of the permissive preflight answer: what the browser asked for,
/// echoed back, plus the freshness hint and the `Vary` of the three headers the
/// answer depends on.
///
/// No CORS configuration is consulted, because no upstream is resolved: the
/// answer is permissive so that the actual request is the one that gets
/// rejected, where the tenant context is available.
#[must_use]
pub fn preflight_headers(request: &HeaderMap) -> HeaderMap {
    let mut answer = HeaderMap::new();
    if let Some(origin) = header(request, ORIGIN) {
        put(&mut answer, ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    if let Some(method) = header(request, ACCESS_CONTROL_REQUEST_METHOD) {
        put(&mut answer, ACCESS_CONTROL_ALLOW_METHODS, method);
    }
    if let Some(requested) = header(request, ACCESS_CONTROL_REQUEST_HEADERS) {
        put(&mut answer, ACCESS_CONTROL_ALLOW_HEADERS, requested);
    }
    answer.insert(
        ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(MAX_AGE_SECS),
    );
    answer.insert(VARY, HeaderValue::from_static(PREFLIGHT_VARY));
    answer
}

// ---------------------------------------------------------------------------
// Layer merge
// ---------------------------------------------------------------------------

/// One layer of the CORS hierarchy: the `cors` block of one resolved resource,
/// in ancestor order (`upstream` before `route`), the way
/// [`crate::domain::merger`] orders its own layers.
#[derive(Debug, Clone, Copy)]
pub struct CorsLayer<'a> {
    cors: Option<&'a CorsConfig>,
}

impl<'a> CorsLayer<'a> {
    /// Builds a layer from its `cors` block, if any.
    #[must_use]
    pub fn new(cors: Option<&'a CorsConfig>) -> Self {
        Self { cors }
    }

    /// The upstream layer.
    #[must_use]
    pub fn upstream(upstream: &'a Upstream) -> Self {
        Self::new(upstream.cors.as_ref())
    }

    /// The route layer.
    #[must_use]
    pub fn route(route: &'a Route) -> Self {
        Self::new(route.cors.as_ref())
    }
}

/// The CORS policy in force for one request, after the layers merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveCors {
    /// Sharing mode of the layer that produced the policy.
    pub sharing: SharingMode,
    /// Origins the browser may call from; `*` allows any of them.
    pub allowed_origins: Vec<String>,
    /// Methods the browser may ask for.
    pub allowed_methods: Vec<String>,
    /// Response headers exposed to the browser's script.
    pub expose_headers: Vec<String>,
    /// Whether the browser may send credentials along.
    pub allow_credentials: bool,
    /// Whether CORS handling is on for the resource the policy came from.
    enabled: bool,
}

impl EffectiveCors {
    /// The policy of a single `cors` block.
    fn of(config: &CorsConfig) -> Self {
        Self {
            sharing: config.sharing,
            enabled: config.enabled,
            allowed_origins: config.allowed_origins.clone(),
            allowed_methods: config.allowed_methods.clone(),
            expose_headers: config.expose_headers.clone(),
            allow_credentials: config.allow_credentials,
        }
    }

    /// `true` when `origin` is in the allowed list, per ADR-0004 "Origin
    /// Matching": an exact, port- and protocol-sensitive match, or the
    /// wildcard. No patterns, so `https://evil.com.example.com` never matches
    /// `https://app.example.com`.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        let origin = normalize_origin(origin);
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || normalize_origin(allowed) == origin)
    }

    /// `true` when `method` is in the allowed list.
    ///
    /// The list is the browser-facing policy ADR-0004 spells out (`GET` and
    /// `HEAD` are separate entries of its enum), so `HEAD` is *not* folded into
    /// `GET` here the way the route match folds it.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// Merges the layers, most ancestral first: `upstream` → `route`.
///
/// Returns `None` when no layer declares a `cors` block, or when the policy in
/// force is disabled, which is not an error: a resource without CORS simply
/// does not answer cross-origin requests, and its responses carry no CORS
/// header at all.
#[must_use]
pub fn merge(layers: &[CorsLayer<'_>]) -> Option<EffectiveCors> {
    let mut effective: Option<EffectiveCors> = None;
    for layer in layers {
        effective = match (effective, layer.cors) {
            (None, None) => None,
            (None, Some(config)) => Some(EffectiveCors::of(config)),
            (Some(parent), None) => Some(parent),
            (Some(parent), Some(child)) => Some(match parent.sharing {
                // `private`: the block is not inherited, so a descendant that
                // declares its own configuration replaces it outright.
                SharingMode::Private => EffectiveCors::of(child),
                // `inherit`: the descendant gets the ancestor's policy *plus*
                // its own — ADR-0004 unions the origins, and the same union is
                // what the methods and exposed headers need to stay open.
                SharingMode::Inherit => union(parent, child),
                // `enforce`: the ancestor's configuration is forced onto the
                // layers below, and a descendant cannot add origins to it.
                SharingMode::Enforce => parent,
            }),
        };
    }
    effective.filter(|policy| policy.enabled)
}

/// The CORS policy of an upstream and of the route that matched it, in the
/// order the proxy resolves them in.
#[must_use]
pub fn for_upstream_route(upstream: &Upstream, route: Option<&Route>) -> Option<EffectiveCors> {
    let mut layers = vec![CorsLayer::upstream(upstream)];
    if let Some(route) = route {
        layers.push(CorsLayer::route(route));
    }
    merge(&layers)
}

/// Unions the configuration in force with the one the descendant declares: the
/// add-only union ADR-0004 describes for `inherit`.
fn union(parent: EffectiveCors, child: &CorsConfig) -> EffectiveCors {
    let child = EffectiveCors::of(child);
    EffectiveCors {
        sharing: parent.sharing,
        // The descendant decides for the resources it governs, so its switch
        // can turn CORS on where the ancestor left it off.
        enabled: parent.enabled || child.enabled,
        allowed_origins: unioned(&parent.allowed_origins, &child.allowed_origins),
        allowed_methods: unioned(&parent.allowed_methods, &child.allowed_methods),
        expose_headers: unioned(&parent.expose_headers, &child.expose_headers),
        // Either layer allowing credentials is enough: the union only ever
        // widens what the browser may send.
        allow_credentials: parent.allow_credentials || child.allow_credentials,
    }
}

/// The add-only union of two lists, ancestors first: a descendant adds to a
/// policy it inherits, it never removes from it.
fn unioned(parent: &[String], child: &[String]) -> Vec<String> {
    let mut merged = parent.to_vec();
    for value in child {
        if !merged
            .iter()
            .any(|known| known.eq_ignore_ascii_case(value) || *known == "*")
        {
            merged.push(value.clone());
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// Actual requests
// ---------------------------------------------------------------------------

/// The `Origin` of a request, normalized for the comparison
/// [`EffectiveCors::allows_origin`] performs.
#[must_use]
pub fn origin_of(headers: &HeaderMap) -> Option<String> {
    header(headers, ORIGIN).map(normalize_origin)
}

/// Normalizes an origin for the exact match: trimmed and lower-cased, because a
/// scheme and a host are case-insensitive while the port and the protocol stay
/// part of the comparison (`https://app.example.com:8443` is a different origin
/// from `https://app.example.com`, and so is `http://app.example.com`).
#[must_use]
pub fn normalize_origin(origin: &str) -> String {
    origin.trim().to_ascii_lowercase()
}

/// The CORS grant of one resolved request: the policy in force and the origin
/// the request came from, or nothing at all when the resource has no CORS
/// policy — in which case the response carries no CORS header and the request
/// is never rejected for one.
#[derive(Debug, Clone)]
pub struct CorsGrant {
    policy: EffectiveCors,
    origin: Option<String>,
}

impl CorsGrant {
    /// Builds the grant of a resolved request.
    #[must_use]
    pub fn new(policy: EffectiveCors, origin: Option<String>) -> Self {
        Self { policy, origin }
    }

    /// Validates an actual request against the policy
    /// ([ADR-0004](../../../docs/ADR/0004-cors.md) "Actual Request Handling"):
    /// the origin first, then the method.
    ///
    /// A request without an `Origin` is not cross-origin and passes whatever
    /// the policy says, since the browser applies the policy, not the caller.
    ///
    /// # Errors
    /// [`ProxyError::CorsOriginNotAllowed`] for an origin outside
    /// `allowed_origins` and [`ProxyError::CorsMethodNotAllowed`] for a method
    /// outside `allowed_methods`.
    pub fn check(&self, method: &str) -> Result<(), ProxyError> {
        let Some(origin) = self.origin.as_deref() else {
            return Ok(());
        };
        if !self.policy.allows_origin(origin) {
            return Err(ProxyError::CorsOriginNotAllowed {
                origin: origin.to_owned(),
            });
        }
        if !self.policy.allows_method(method) {
            return Err(ProxyError::CorsMethodNotAllowed {
                method: method.to_owned(),
            });
        }
        Ok(())
    }

    /// Adds the CORS headers of the response to `headers`.
    pub fn apply(&self, headers: &mut HeaderMap) {
        grant_headers(headers, &self.policy, self.origin.as_deref());
    }
}

/// Adds the CORS headers of a governed actual response to `headers`.
///
/// The allowed origin is echoed back rather than answered with `*`, which is
/// what a credentials-bearing response needs anyway. `Vary: Origin` is added
/// because the answer depends on the caller's origin, appended to whatever
/// `Vary` the upstream already sent, so a shared cache never hands a CORS
/// answer to a request that asked for none.
fn grant_headers(headers: &mut HeaderMap, policy: &EffectiveCors, origin: Option<&str>) {
    let Some(origin) = origin else {
        return;
    };
    add_vary_origin(headers);
    put(
        headers,
        ACCESS_CONTROL_ALLOW_ORIGIN,
        &normalize_origin(origin),
    );
    if !policy.expose_headers.is_empty() {
        put(
            headers,
            ACCESS_CONTROL_EXPOSE_HEADERS,
            &policy.expose_headers.join(", "),
        );
    }
    if policy.allow_credentials {
        headers.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static(ALLOW_CREDENTIALS),
        );
    }
}

/// Appends `Origin` to the `Vary` of `headers`, unless it is already listed.
///
/// The upstream's own `Vary` entries are kept: a second `Vary` header line is
/// equivalent to a joined one, and the upstream may well have been varying on
/// `Accept-Encoding` already.
pub fn add_vary_origin(headers: &mut HeaderMap) {
    let listed = headers
        .get_all(VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("origin"))
        });
    if !listed {
        headers.append(VARY, HeaderValue::from_static("Origin"));
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// The trimmed text of `name` in `headers`, when it is a visible-ASCII,
/// non-empty value.
fn header(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Writes one response header, replacing whatever the upstream sent for it, and
/// skipping a value that is not a valid HTTP header value.
fn put(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use http::header::{
        ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS,
        ACCESS_CONTROL_MAX_AGE, VARY,
    };

    use http::{HeaderMap, HeaderName, HeaderValue};

    use super::{
        CorsGrant, CorsLayer, EffectiveCors, add_vary_origin, is_preflight, merge, origin_of,
        preflight_headers,
    };
    use crate::domain::model::{CorsConfig, SharingMode};
    use crate::domain::proxy::ProxyError;

    /// A `cors` block with the given sharing mode and origins.
    fn cors(sharing: SharingMode, enabled: bool, origins: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing,
            enabled,
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    fn policy(origins: &[&str]) -> EffectiveCors {
        EffectiveCors::of(&cors(SharingMode::Private, true, origins))
    }

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        entries
            .iter()
            .fold(HeaderMap::new(), |mut headers, (name, value)| {
                let name: HeaderName = name.parse().unwrap();
                let value: HeaderValue = value.parse().unwrap();
                headers.insert(name, value);
                headers
            })
    }

    #[test]
    fn a_preflight_is_an_options_with_an_origin_and_a_requested_method() {
        let preflight = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ]);
        assert!(is_preflight("OPTIONS", &preflight));
        assert!(is_preflight("options", &preflight));

        // Without the requested method it is an ordinary `OPTIONS`, which the
        // proxy path serves like any other request.
        assert!(!is_preflight(
            "OPTIONS",
            &headers(&[("origin", "https://app.example.com")])
        ));
        // And without an origin it is not cross-origin at all.
        assert!(!is_preflight(
            "OPTIONS",
            &headers(&[("access-control-request-method", "POST")])
        ));
        assert!(!is_preflight("GET", &preflight));
    }

    #[test]
    fn the_preflight_answer_echoes_what_the_browser_asked_for() {
        let request = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "Content-Type, Authorization",
            ),
        ]);
        let answer = preflight_headers(&request);

        assert_eq!(
            answer[ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
        assert_eq!(answer[ACCESS_CONTROL_ALLOW_METHODS], "POST");
        assert_eq!(
            answer[ACCESS_CONTROL_ALLOW_HEADERS],
            "Content-Type, Authorization"
        );
        assert_eq!(answer[ACCESS_CONTROL_MAX_AGE], super::MAX_AGE_SECS);
        assert_eq!(
            answer[VARY],
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
        );
    }

    #[test]
    fn a_preflight_without_a_requested_header_list_omits_it() {
        let request = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "GET"),
        ]);
        let answer = preflight_headers(&request);
        assert!(!answer.contains_key(ACCESS_CONTROL_ALLOW_HEADERS));
    }

    #[test]
    fn origins_match_exactly_on_scheme_host_and_port() {
        let allowed = policy(&["https://app.example.com"]);
        assert!(allowed.allows_origin("https://app.example.com"));
        assert!(allowed.allows_origin("https://App.Example.COM"));

        // Port-sensitive and protocol-sensitive, per ADR-0004.
        assert!(!allowed.allows_origin("https://app.example.com:8443"));
        assert!(!allowed.allows_origin("http://app.example.com"));
        // And a suffix is not a match: no patterns, so no bypass.
        assert!(!allowed.allows_origin("https://evil.com.example.com"));
        assert!(!allowed.allows_origin("https://app.example.com.evil.com"));
    }

    #[test]
    fn the_wildcard_origin_allows_every_origin() {
        let allowed = policy(&["*"]);
        assert!(allowed.allows_origin("https://app.example.com"));
        assert!(allowed.allows_origin("http://localhost:5173"));
        // An absent (or blank) origin is not a value `allows_origin` ever sees:
        // [`super::origin_of`] drops it, and the request is not cross-origin.
        assert_eq!(origin_of(&headers(&[("origin", "  ")])), None);
    }

    #[test]
    fn methods_match_case_insensitively() {
        let allowed = policy(&["*"]);
        assert!(allowed.allows_method("GET"));
        assert!(allowed.allows_method("get"));
        assert!(!allowed.allows_method("DELETE"));
    }

    #[test]
    fn an_unconfigured_or_disabled_policy_is_no_policy_at_all() {
        assert_eq!(merge(&[CorsLayer::new(None)]), None);
        assert_eq!(
            merge(&[CorsLayer::new(Some(&cors(
                SharingMode::Private,
                false,
                &["https://app.example.com"]
            )))]),
            None
        );
    }

    #[test]
    fn a_private_route_block_replaces_the_upstream_one() {
        let upstream = cors(SharingMode::Private, true, &["https://app.example.com"]);
        let route = cors(SharingMode::Private, true, &["https://admin.example.com"]);

        let merged = merge(&[
            CorsLayer::new(Some(&upstream)),
            CorsLayer::new(Some(&route)),
        ])
        .expect("the route declares CORS");

        assert_eq!(merged.sharing, SharingMode::Private);
        assert_eq!(merged.allowed_origins, vec!["https://admin.example.com"]);
    }

    #[test]
    fn an_inherited_route_block_unions_its_origins_with_the_upstream_ones() {
        let upstream = cors(SharingMode::Inherit, true, &["https://app.example.com"]);
        let mut route = cors(SharingMode::Inherit, false, &["https://admin.example.com"]);
        route.allow_credentials = true;
        route.expose_headers = vec!["X-Request-ID".to_owned()];

        let merged = merge(&[
            CorsLayer::new(Some(&upstream)),
            CorsLayer::new(Some(&route)),
        ])
        .expect("CORS is inherited");

        assert_eq!(
            merged.allowed_origins,
            vec![
                "https://app.example.com".to_owned(),
                "https://admin.example.com".to_owned(),
            ]
        );
        // The inherited policy only ever widens.
        assert!(merged.enabled);
        assert!(merged.allow_credentials);
        assert_eq!(merged.expose_headers, vec!["X-Request-ID".to_owned()]);
    }

    #[test]
    fn an_enforced_upstream_block_cannot_be_widened_by_the_route() {
        let upstream = cors(SharingMode::Enforce, true, &["https://app.example.com"]);
        let route = cors(SharingMode::Private, true, &["https://evil.example.com"]);

        let merged = merge(&[
            CorsLayer::new(Some(&upstream)),
            CorsLayer::new(Some(&route)),
        ])
        .expect("the enforced policy stands");

        assert_eq!(merged.sharing, SharingMode::Enforce);
        assert_eq!(merged.allowed_origins, vec!["https://app.example.com"]);
        assert!(!merged.allows_origin("https://evil.example.com"));
    }

    #[test]
    fn a_route_without_a_block_keeps_the_upstream_policy() {
        let upstream = cors(SharingMode::Private, true, &["https://app.example.com"]);
        let merged = merge(&[CorsLayer::new(Some(&upstream)), CorsLayer::new(None)])
            .expect("the upstream policy stands");
        assert_eq!(merged.allowed_origins, vec!["https://app.example.com"]);
    }

    #[test]
    fn a_unioned_wildcard_cannot_be_narrowed_back() {
        let upstream = cors(SharingMode::Inherit, true, &["*"]);
        let route = cors(SharingMode::Inherit, false, &["https://app.example.com"]);
        let merged = merge(&[
            CorsLayer::new(Some(&upstream)),
            CorsLayer::new(Some(&route)),
        ])
        .expect("CORS is inherited");
        assert!(merged.allows_origin("https://anything.example.com"));
    }

    #[test]
    fn the_actual_response_carries_the_cors_headers_of_the_policy() {
        let mut allowed = policy(&["https://app.example.com"]);
        allowed.expose_headers = vec![
            "X-Request-ID".to_owned(),
            "X-RateLimit-Remaining".to_owned(),
        ];
        allowed.allow_credentials = true;

        let grant = CorsGrant::new(allowed, Some("https://app.example.com".to_owned()));
        let mut headers = HeaderMap::new();
        grant.apply(&mut headers);
        assert_eq!(
            headers[ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
        assert_eq!(
            headers[ACCESS_CONTROL_EXPOSE_HEADERS],
            "X-Request-ID, X-RateLimit-Remaining"
        );
        assert_eq!(headers[ACCESS_CONTROL_ALLOW_CREDENTIALS], "true");
        assert_eq!(headers[VARY], "Origin");
    }

    #[test]
    fn a_request_without_an_origin_gets_no_cors_grant_at_all() {
        let grant = CorsGrant::new(policy(&["*"]), None);
        let mut headers = HeaderMap::new();
        grant.apply(&mut headers);
        assert!(headers.is_empty(), "nothing to grant without an origin");
        assert!(
            !headers.contains_key(VARY),
            "`Vary: Origin` is the caller's"
        );
    }

    #[test]
    fn a_request_without_an_origin_is_never_rejected() {
        let grant = CorsGrant::new(policy(&["https://app.example.com"]), None);
        assert!(grant.check("GET").is_ok());
        assert!(
            grant.check("DELETE").is_ok(),
            "CORS governs cross-origin only"
        );
    }

    #[test]
    fn a_disallowed_origin_is_a_403_naming_the_origin() {
        let grant = CorsGrant::new(
            policy(&["https://app.example.com"]),
            Some("https://evil.example.com".to_owned()),
        );

        let error = grant.check("GET").unwrap_err();
        assert_eq!(error.http_status(), 403);
        match &error {
            ProxyError::CorsOriginNotAllowed { origin } => {
                assert_eq!(origin, "https://evil.example.com");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let canonical = error.canonical();
        assert_eq!(canonical.status_code(), 403);
        assert!(
            canonical.resource_type().is_some_and(
                |resource_type| resource_type.ends_with("cf.oagw.cors.origin_not_allowed.v1~")
            ),
            "{canonical:?}"
        );
    }

    #[test]
    fn a_disallowed_method_is_a_403_naming_the_method() {
        let grant = CorsGrant::new(
            policy(&["https://app.example.com"]),
            Some("https://app.example.com".to_owned()),
        );

        let error = grant.check("DELETE").unwrap_err();
        assert_eq!(error.http_status(), 403);
        match error {
            ProxyError::CorsMethodNotAllowed { method } => assert_eq!(method, "DELETE"),
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            grant
                .check("DELETE")
                .unwrap_err()
                .canonical()
                .resource_type()
                .is_some_and(
                    |resource_type| resource_type.ends_with("cf.oagw.cors.method_not_allowed.v1~")
                )
        );
    }

    #[test]
    fn an_allowed_request_passes_the_check() {
        let grant = CorsGrant::new(
            policy(&["https://app.example.com"]),
            Some("https://app.example.com".to_owned()),
        );
        assert!(grant.check("GET").is_ok());
        assert!(grant.check("get").is_ok());
    }

    #[test]
    fn the_upstream_vary_is_kept_and_origin_appended_once() {
        let mut headers = headers(&[("vary", "Accept-Encoding")]);
        add_vary_origin(&mut headers);
        let vary: Vec<_> = headers
            .get_all(VARY)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect();
        assert_eq!(vary, vec!["Accept-Encoding", "Origin"]);

        add_vary_origin(&mut headers);
        assert_eq!(vary.len(), 2, "Origin must not be listed twice");
    }

    #[test]
    fn an_origin_the_upstream_sent_is_replaced_by_the_gateway_one() {
        let mut headers = headers(&[(
            "access-control-allow-origin",
            "https://upstream.example.com",
        )]);
        CorsGrant::new(
            policy(&["https://app.example.com"]),
            Some("https://app.example.com".to_owned()),
        )
        .apply(&mut headers);
        assert_eq!(
            headers[ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
    }

    #[test]
    fn the_request_origin_is_normalized_for_the_grant() {
        assert_eq!(
            origin_of(&headers(&[("origin", "  HTTPS://App.Example.COM ")])).as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(origin_of(&HeaderMap::new()), None);
        assert_eq!(origin_of(&headers(&[("origin", "")])), None);
    }
}
