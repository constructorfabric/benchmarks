//! CORS: a built-in handler of the proxy, not a plugin (ADR-0004).
//!
//! Two very different requests share the `cors` configuration:
//!
//! * **Preflight** (`OPTIONS` + `Origin` +
//!   `Access-Control-Request-Method`) is answered *locally* with a permissive
//!   204 that echoes what the browser asked for (ADR-0004 "Preflight Request
//!   Handling"). A preflight carries no credentials, so there is no tenant
//!   context to resolve an upstream with and nothing to validate yet: origin
//!   and method enforcement is deferred to the actual request, and ADR-0004
//!   explicitly accepts that a preflight for an unknown alias is answered
//!   rather than rejected. The response is `X-OAGW-Error-Source: gateway` like
//!   every gateway-produced response (ADR-0007), and it is stamped with `Vary`
//!   so no cache can serve it for a different origin, method or header set.
//!   ADR-0004 leaves preflights subject to infrastructure-level controls (edge
//!   rate limiting, WAF). The transport answers them *before* it requires a
//!   `SecurityContext`: a preflight carries no credentials (WHATWG Fetch), so a
//!   transport that authenticated first would turn every preflight into a 401
//!   and no browser could ever use the gateway. The answer is data-free — it
//!   echoes only the `Access-Control-Request-*` headers the caller sent — so
//!   answering it unauthenticated buys a caller nothing.
//! * **Actual** requests are validated once the upstream is resolved: a
//!   disallowed origin is a 403 `cf.oagw.cors.origin_not_allowed.v1`, a
//!   disallowed method a 403 `cf.oagw.cors.method_not_allowed.v1` (ADR-0004
//!   "Error Responses"). A request without an `Origin` header is not a CORS
//!   request at all and takes neither check nor header.
//!
//! # A disabled configuration
//!
//! `cors.enabled` decides whether the gateway polices CORS at all. A
//! **disabled** configuration means the gateway stays out of the conversation
//! entirely: [`cors_response_headers`] emits no `Access-Control-*` header for
//! any request and raises no 403, so a cross-origin caller is blocked by the
//! browser's own same-origin policy — the *absence* of
//! `Access-Control-Allow-Origin` in the response — and not by a gateway
//! rejection. That is the "deny by default" ADR-0004 asks for under "Security
//! defaults" (CORS disabled unless explicitly enabled): the gateway neither
//! vouches for an origin nor owes one it never agreed to serve an explanation.
//!
//! A preflight keeps its permissive 204 either way, because it is answered
//! before any configuration is resolved and carries nothing but the echo of the
//! request; what a disabled configuration removes is the enforcement *and* the
//! headers of the actual request.
//!
//! # Effective configuration
//!
//! [`effective_cors`] folds the `cors` field along the tenant chain, the way
//! [`crate::domain::policy::rate_limit::effective_rate_limit`] folds
//! `rate_limit`: the selected upstream's own configuration decides, amended by
//! the ancestor upstreams that share its alias. ADR-0004 defines only the
//! origin merge, so the whole rule is spelled out here:
//!
//! | Ancestor `sharing` | Effect on the descendant |
//! |---|---|
//! | `private` | invisible: contributes nothing |
//! | `inherit` | union of origins, expose headers and methods; the descendant's own `enabled` and `allow_credentials` win |
//! | `enforce` | the ancestor's configuration replaces the descendant's entirely: it can neither widen the origin set nor opt out |
//!
//! `RouteSpec` has no `cors` field in the schema the crate is bound to
//! (`docs/schemas/route.v1.schema.json` declares `match`, `plugins`,
//! `rate_limit`, `tags`, `upstream_id`), so CORS is an upstream-only policy and
//! the route contributes nothing. The seam is kept in
//! [`effective_cors`]'s signature shape (a chain walk plus a lookup), so a route
//! layer can be added without touching the callers.
//!
//! # Documented deviations
//!
//! * **Origins are matched byte-for-byte.** ADR-0004 requires port- and
//!   protocol-sensitive matching and forbids regex patterns; the strictest
//!   reading is exact string equality on the header value, which additionally
//!   rejects a differently-cased spelling of the same origin. Browsers emit
//!   lowercase schemes and hosts, so a legitimate caller is unaffected, and a
//!   caller that spells its origin differently can never slip past a rule that
//!   was written for a differently-cased origin.
//! * **`Access-Control-Allow-Origin` always echoes the request origin.**
//!   ADR-0004's preflight response echoes it; the actual request does the same
//!   instead of emitting `*` for a wildcard configuration, so a wildcard
//!   configuration and a credential-bearing one produce the same header shape
//!   and no response can ever combine `*` with
//!   `Access-Control-Allow-Credentials: true` (WHATWG Fetch forbids it, and
//!   [`crate::domain::types::validate_cors`] rejects the configuration).

use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue, Method};
use uuid::Uuid;

use crate::domain::types::{CorsConfig, CorsMethod, SharingMode, Upstream};
use crate::error::OagwError;

/// `Access-Control-Allow-Origin`.
const ALLOW_ORIGIN: &str = "access-control-allow-origin";
/// `Access-Control-Allow-Methods`.
const ALLOW_METHODS: &str = "access-control-allow-methods";
/// `Access-Control-Allow-Headers`.
const ALLOW_HEADERS: &str = "access-control-allow-headers";
/// `Access-Control-Expose-Headers`.
const EXPOSE_HEADERS: &str = "access-control-expose-headers";
/// `Access-Control-Allow-Credentials`.
const ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
/// `Access-Control-Max-Age` (ADR-0004: one day).
const MAX_AGE: &str = "access-control-max-age";
/// `Access-Control-Max-Age` value (ADR-0004 "Preflight Request Handling").
const MAX_AGE_SECS: &str = "86400";
/// `Origin`.
const ORIGIN: &str = "origin";
/// `Access-Control-Request-Method`.
const REQUEST_METHOD: &str = "access-control-request-method";
/// `Access-Control-Request-Headers`.
const REQUEST_HEADERS: &str = "access-control-request-headers";
/// `Vary`.
const VARY: &str = "vary";
/// `Vary` of an actual request (ADR-0004 "Security Considerations": always, so
/// no cache can serve one origin's response to another).
const VARY_ACTUAL: &str = "Origin";
/// `Vary` of a preflight: every header the answer was echoed from.
const VARY_PREFLIGHT: &str = "Origin, Access-Control-Request-Method, \
     Access-Control-Request-Headers";

/// What a preflight asked for, as far as the answer depends on it.
///
/// Every field is optional: a browser may omit
/// `Access-Control-Request-Headers` when it needs none, and the answer echoes
/// only what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsPreflight {
    /// The `Origin` header value.
    pub origin: Option<String>,
    /// The `Access-Control-Request-Method` header value.
    pub request_method: Option<String>,
    /// The `Access-Control-Request-Headers` header value.
    pub request_headers: Option<String>,
}

impl CorsPreflight {
    /// Read the three headers that make a request a preflight.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            origin: header(headers, ORIGIN),
            request_method: header(headers, REQUEST_METHOD),
            request_headers: header(headers, REQUEST_HEADERS),
        }
    }
}

/// An actual (non-preflight) request, as far as the CORS decision depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsRequest {
    /// The `Origin` header value; `None` means "not a CORS request".
    pub origin: Option<String>,
    /// The request method token.
    pub method: String,
}

/// `true` when the request is a CORS preflight (ADR-0004 "Preflight Request
/// Handling"): `OPTIONS`, with an `Origin` **and** an
/// `Access-Control-Request-Method`.
///
/// An `OPTIONS` without both headers is an ordinary request and is routed like
/// any other, so an upstream that serves `OPTIONS` itself keeps doing so.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(ORIGIN)
        && headers.contains_key(REQUEST_METHOD)
}

/// The permissive 204 a preflight is answered with (ADR-0004 "Preflight Request
/// Handling").
///
/// The origin, method and headers are echoed verbatim rather than validated:
/// ADR-0004 defers validation to the actual request, which is the only one that
/// can be tied to a tenant and an upstream.
#[must_use]
pub fn preflight_response(request: &CorsPreflight) -> PreflightResponse {
    let mut headers = HeaderMap::with_capacity(6);

    // The browser sent an `Origin` (otherwise this is not a preflight), and a
    // value that is not a valid header value cannot be echoed: the answer then
    // carries no origin at all, which no browser will accept.
    if let Some(origin) = request.origin.as_deref()
        && let Ok(value) = HeaderValue::from_str(origin)
    {
        headers.insert(ALLOW_ORIGIN, value);
    }
    if let Some(method) = request.request_method.as_deref()
        && let Ok(value) = HeaderValue::from_str(method)
    {
        headers.insert(ALLOW_METHODS, value);
    }
    if let Some(request_headers) = request.request_headers.as_deref()
        && let Ok(value) = HeaderValue::from_str(request_headers)
    {
        headers.insert(ALLOW_HEADERS, value);
    }
    headers.insert(MAX_AGE, HeaderValue::from_static(MAX_AGE_SECS));
    headers.insert(VARY, HeaderValue::from_static(VARY_PREFLIGHT));
    headers.insert(
        crate::error::error_source_header_name(),
        crate::error::error_source_header_value(crate::error::ERROR_SOURCE_GATEWAY),
    );

    PreflightResponse::no_content(headers)
}

/// A preflight answer: an empty 204 body with the headers of
/// [`preflight_response`].
///
/// A named type rather than a bare `axum::Response` so the data-plane seam
/// documents what it is handing out.
#[derive(Debug)]
pub struct PreflightResponse(axum::response::Response);

impl PreflightResponse {
    /// Build the 204 with the given headers.
    fn no_content(headers: HeaderMap) -> Self {
        let mut response = axum::response::Response::new(axum::body::Body::empty());
        *response.status_mut() = http::StatusCode::NO_CONTENT;
        *response.headers_mut() = headers;
        Self(response)
    }

    /// Consume into the transport response.
    #[must_use]
    pub fn into_response(self) -> axum::response::Response {
        self.0
    }

    /// The status of the answer, for tests and the audit trail.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        self.0.status()
    }

    /// The headers of the answer.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.0.headers()
    }
}

/// Fold the `cors` configuration of the chain into the one that applies.
///
/// `chain` is the ancestor chain, nearest first and including the calling
/// tenant; `lookup` resolves `(tenant, alias)` in the upstream store. See the
/// [module documentation](self#effective-configuration) for the merge rule.
///
/// `None` when nothing in the chain enables CORS: the request then takes no
/// CORS check and carries no CORS header (ADR-0004 "Security Considerations":
/// deny by default).
#[must_use]
pub fn effective_cors<F>(chain: &[Uuid], selected: &Upstream, mut lookup: F) -> Option<CorsConfig>
where
    F: FnMut(Uuid, &str) -> Option<Arc<Upstream>>,
{
    // Ancestors above the owner of the alias only: a nearer tenant's
    // configuration is the one that shadows them (DESIGN §3.5 "Shadowing
    // Behavior").
    let ancestors = match chain
        .iter()
        .position(|tenant| *tenant == selected.tenant_id)
    {
        Some(position) => &chain[position + 1..],
        None => chain,
    };

    // Fold root → nearest, so every layer answers to the one below it: an
    // ancestor's `sharing` says how *it* bears on what sits underneath it.
    let mut effective: Option<CorsConfig> = None;
    for tenant in ancestors.iter().rev() {
        let Some(ancestor) = lookup(*tenant, &selected.alias) else {
            continue;
        };
        if let Some(config) = ancestor.spec.cors.as_ref() {
            effective = Some(merge_over_inherited(effective, config));
        }
    }
    if let Some(config) = selected.spec.cors.as_ref() {
        effective = Some(merge_over_inherited(effective, config));
    }

    effective
}

/// Merge a nearer CORS configuration over the one it inherits.
///
/// The *inherited* configuration's [`SharingMode`] decides, because it is the
/// declaration of how that layer intends to be inherited:
///
/// * `private` — invisible, the nearer layer stands alone;
/// * `inherit` — the nearer layer is unioned over it, and decides whether CORS
///   is on at all and whether credentials flow;
/// * `enforce` — it replaces the nearer layer entirely, which can then neither
///   widen the origin set nor opt out.
fn merge_over_inherited(inherited: Option<CorsConfig>, own: &CorsConfig) -> CorsConfig {
    let Some(inherited) = inherited else {
        return own.clone();
    };

    match inherited.sharing {
        SharingMode::Private => own.clone(),
        // ADR-0004 "Hierarchical Configuration": the child's origins are
        // unioned with the parent's.
        SharingMode::Inherit => CorsConfig {
            sharing: own.sharing,
            enabled: own.enabled,
            allowed_origins: union(&inherited.allowed_origins, &own.allowed_origins),
            allowed_methods: union_methods(&inherited.allowed_methods, &own.allowed_methods),
            expose_headers: union(&inherited.expose_headers, &own.expose_headers),
            allow_credentials: own.allow_credentials,
        },
        // The descendant can neither widen the origin set nor opt out.
        SharingMode::Enforce => inherited,
    }
}

/// The union of two lists, first occurrence first, without duplicates.
fn union(ancestors: &[String], own: &[String]) -> Vec<String> {
    let mut merged = ancestors.to_vec();
    for value in own {
        if !merged.contains(value) {
            merged.push(value.clone());
        }
    }
    merged
}

/// The union of two method lists, in configuration order.
fn union_methods(ancestors: &[CorsMethod], own: &[CorsMethod]) -> Vec<CorsMethod> {
    let mut merged = ancestors.to_vec();
    for method in own {
        if !merged.contains(method) {
            merged.push(*method);
        }
    }
    merged
}

/// Validate an actual cross-origin request and produce its CORS response
/// headers.
///
/// Returns the headers to add to the response that is returned to the caller —
/// the gateway never forwards them to the upstream, whose response must stay
/// exactly what the upstream produced. An empty map is returned when the
/// request carries no `Origin`: it is not a CORS request.
///
/// A configuration whose [`CorsConfig::enabled`] is `false` polices nothing:
/// the gateway emits no `Access-Control-*` header at all and raises no 403, so
/// a cross-origin caller is blocked by the browser's same-origin policy (the
/// missing `Access-Control-Allow-Origin`) rather than by a gateway rejection —
/// the deny-by-default of ADR-0004 "Security defaults". The check below the
/// `enabled` gate is therefore unreachable for a disabled configuration.
///
/// # Errors
/// * [`crate::error::OagwErrorKind::CorsOriginNotAllowed`] — 403, the origin is
///   not in `allowed_origins`.
/// * [`crate::error::OagwErrorKind::CorsMethodNotAllowed`] — 403, the method is
///   not in `allowed_methods`.
pub fn cors_response_headers(
    config: &CorsConfig,
    request: &CorsRequest,
) -> Result<HeaderMap, OagwError> {
    let mut headers = HeaderMap::new();

    // A disabled configuration is not a policy, it is the absence of one: no
    // header, no rejection, for any origin and any method. Nothing below this
    // gate may run, or the gateway would be enforcing a configuration that
    // explicitly opted out.
    if !config.enabled {
        return Ok(headers);
    }

    let Some(origin) = request.origin.as_deref() else {
        return Ok(headers);
    };

    if !origin_allowed(config, origin) {
        return Err(OagwError::cors_origin_not_allowed(format!(
            "origin '{origin}' is not in the allowed origins of this upstream"
        ))
        .with_extension("origin", serde_json::json!(origin)));
    }

    if !method_allowed(config, &request.method) {
        return Err(OagwError::cors_method_not_allowed(format!(
            "method '{}' is not in the allowed methods of this upstream",
            request.method
        ))
        .with_extension("method", serde_json::json!(request.method)));
    }

    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(ALLOW_ORIGIN, value);
    }
    if !config.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&config.expose_headers.join(", "))
    {
        headers.insert(EXPOSE_HEADERS, value);
    }
    if config.allow_credentials {
        headers.insert(ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    }
    headers.insert(VARY, HeaderValue::from_static(VARY_ACTUAL));

    Ok(headers)
}

/// Whether `origin` is allowed: exact match, or the `*` wildcard.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed == origin)
}

/// Whether the request method is allowed (case-insensitive, like the wire
/// tokens are written).
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &str) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.as_str().eq_ignore_ascii_case(method))
}

/// The data-plane CORS hook (ADR-0004): stateless, so one serves the process.
///
/// A preflight is answered *always* and permissively, even for an alias nothing
/// resolves (ADR-0004 "Preflight Request Handling"); an actual request is
/// validated against the effective configuration the data plane folded, and
/// takes no CORS header at all when there is none.
#[derive(Debug, Default)]
pub struct CorsService;

impl crate::domain::services::data_plane::CorsHook for CorsService {
    fn preflight(&self, preflight: &CorsPreflight) -> Option<PreflightResponse> {
        Some(preflight_response(preflight))
    }

    fn validate(
        &self,
        _context: &crate::domain::services::data_plane::ProxyContext,
        config: &CorsConfig,
        request: &CorsRequest,
    ) -> Result<HeaderMap, OagwError> {
        cors_response_headers(config, request)
    }
}

/// The value of a request header, if it is a valid UTF-8 string.
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
    headers
        .get(&name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{Endpoint, Protocol, Scheme, ServerConfig, UpstreamSpec};
    use crate::error::OagwErrorKind;

    const TENANT: Uuid = Uuid::from_u128(0x0002);
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x0003);

    fn upstream(alias: &str, tenant: Uuid, cors: Option<CorsConfig>) -> Upstream {
        let mut spec = UpstreamSpec {
            alias: Some(alias.to_owned()),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: 8080,
                }],
            },
            protocol: Protocol::Http,
            cors,
            ..UpstreamSpec::default()
        };
        spec = spec.validate().expect("the upstream spec normalizes");

        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        }
    }

    fn config(sharing: SharingMode, origins: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing,
            enabled: true,
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allowed_methods: vec![CorsMethod::Get, CorsMethod::Post],
            expose_headers: Vec::new(),
            allow_credentials: credentials,
        }
    }

    fn preflight(origin: &str, method: &str, headers: Option<&str>) -> CorsPreflight {
        CorsPreflight {
            origin: Some(origin.to_owned()),
            request_method: Some(method.to_owned()),
            request_headers: headers.map(str::to_owned),
        }
    }

    fn header_str<'a>(headers: &'a HeaderMap, name: &'a str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    #[test]
    fn an_options_with_origin_and_request_method_is_a_preflight() {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, "https://app.example.com".parse().expect("valid"));
        headers.insert(REQUEST_METHOD, "POST".parse().expect("valid"));

        assert!(is_preflight(&Method::OPTIONS, &headers));
    }

    #[test]
    fn a_request_without_the_preflight_headers_is_not_a_preflight() {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, "https://app.example.com".parse().expect("valid"));
        assert!(
            !is_preflight(&Method::OPTIONS, &headers),
            "no Access-Control-Request-Method: an ordinary OPTIONS"
        );
        assert!(
            !is_preflight(&Method::GET, &headers),
            "an actual cross-origin request is not a preflight"
        );
        assert!(!is_preflight(&Method::OPTIONS, &HeaderMap::new()));
    }

    #[test]
    fn the_preflight_answer_is_a_permissive_204() {
        let response = preflight_response(&preflight(
            "https://app.example.com",
            "POST",
            Some("Content-Type, Authorization"),
        ));

        assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            header_str(headers, ALLOW_ORIGIN),
            Some("https://app.example.com"),
            "the requested origin is echoed"
        );
        assert_eq!(header_str(headers, ALLOW_METHODS), Some("POST"));
        assert_eq!(
            header_str(headers, ALLOW_HEADERS),
            Some("Content-Type, Authorization")
        );
        assert_eq!(header_str(headers, MAX_AGE), Some(MAX_AGE_SECS));
        assert_eq!(
            header_str(headers, VARY),
            Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
        );
        assert_eq!(
            header_str(headers, "x-oagw-error-source"),
            Some("gateway"),
            "a gateway-produced answer carries the error source"
        );
    }

    #[test]
    fn the_preflight_answer_omits_what_the_browser_did_not_ask_for() {
        let response = preflight_response(&preflight("https://app.example.com", "GET", None));

        assert!(header_str(response.headers(), ALLOW_HEADERS).is_none());
        assert!(header_str(response.headers(), ALLOW_ORIGIN).is_some());
    }

    #[test]
    fn an_unrepresentable_origin_is_not_echoed() {
        let response = preflight_response(&CorsPreflight {
            origin: Some("https://bad\norigin".to_owned()),
            request_method: Some("GET".to_owned()),
            request_headers: None,
        });

        assert!(header_str(response.headers(), ALLOW_ORIGIN).is_none());
        assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
    }

    #[test]
    fn an_inherited_ancestor_unions_its_origins() {
        let ancestor = upstream(
            "api.vendor.com",
            OTHER_TENANT,
            Some(CorsConfig {
                sharing: SharingMode::Inherit,
                allowed_origins: vec!["https://app.example.com".to_owned()],
                allowed_methods: vec![CorsMethod::Get],
                expose_headers: vec!["X-Request-ID".to_owned()],
                ..config(SharingMode::Inherit, &["https://app.example.com"], false)
            }),
        );
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(CorsConfig {
                allowed_origins: vec!["https://admin.example.com".to_owned()],
                allowed_methods: vec![CorsMethod::Post],
                ..config(SharingMode::Inherit, &["https://admin.example.com"], false)
            }),
        );
        let chain = [TENANT, OTHER_TENANT];

        let effective = effective_cors(&chain, &selected, |tenant, _| {
            (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
        })
        .expect("the upstream enables CORS");

        assert_eq!(
            effective.allowed_origins,
            vec![
                "https://app.example.com".to_owned(),
                "https://admin.example.com".to_owned()
            ],
            "ADR-0004: the child's origins are unioned with the parent's"
        );
        assert_eq!(
            effective.allowed_methods,
            vec![CorsMethod::Get, CorsMethod::Post]
        );
        assert_eq!(effective.expose_headers, vec!["X-Request-ID".to_owned()]);
        assert!(effective.enabled);
    }

    #[test]
    fn an_enforced_ancestor_cannot_be_widened_or_opted_out() {
        let ancestor = upstream(
            "api.vendor.com",
            OTHER_TENANT,
            Some(config(
                SharingMode::Enforce,
                &["https://app.example.com"],
                false,
            )),
        );
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(CorsConfig {
                enabled: false,
                allowed_origins: vec!["*".to_owned()],
                ..config(SharingMode::Inherit, &["*"], false)
            }),
        );

        let effective = effective_cors(&[TENANT, OTHER_TENANT], &selected, |tenant, _| {
            (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
        })
        .expect("the enforced ancestor applies");

        assert_eq!(
            effective.allowed_origins,
            vec!["https://app.example.com".to_owned()],
            "the descendant cannot add origins"
        );
        assert!(effective.enabled, "the descendant cannot opt out");
    }

    #[test]
    fn a_private_ancestor_is_invisible_to_a_descendant() {
        let ancestor = upstream(
            "api.vendor.com",
            OTHER_TENANT,
            Some(config(
                SharingMode::Private,
                &["https://app.example.com"],
                false,
            )),
        );
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(
                SharingMode::Private,
                &["https://admin.example.com"],
                false,
            )),
        );

        let effective = effective_cors(&[TENANT, OTHER_TENANT], &selected, |tenant, _| {
            (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
        })
        .expect("the upstream enables CORS");

        assert_eq!(
            effective.allowed_origins,
            vec!["https://admin.example.com".to_owned()],
            "a private ancestor's configuration is not inherited"
        );
    }

    #[test]
    fn a_descendant_without_configuration_inherits_an_ancestor_that_shares() {
        let ancestor = upstream(
            "api.vendor.com",
            OTHER_TENANT,
            Some(config(
                SharingMode::Enforce,
                &["https://app.example.com"],
                true,
            )),
        );
        let selected = upstream("api.vendor.com", TENANT, None);

        let effective = effective_cors(&[TENANT, OTHER_TENANT], &selected, |tenant, _| {
            (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
        })
        .expect("the enforced ancestor applies to the shadowing descendant");

        assert_eq!(
            effective.allowed_origins,
            vec!["https://app.example.com".to_owned()]
        );
        assert!(effective.allow_credentials);
    }

    #[test]
    fn no_cors_configuration_anywhere_means_no_cors_behaviour() {
        let selected = upstream("api.vendor.com", TENANT, None);

        assert!(
            effective_cors(&[TENANT], &selected, |_, _| None).is_none(),
            "CORS is disabled unless explicitly enabled (ADR-0004)"
        );
    }

    #[test]
    fn a_disabled_configuration_emits_no_header_and_raises_no_rejection() {
        // `enabled: false` with a non-empty allow-list: the list is inert, which
        // is what "deny by default" (ADR-0004 "Security defaults") means.
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(CorsConfig {
                enabled: false,
                ..config(SharingMode::Private, &["https://app.example.com"], false)
            }),
        );

        let effective = effective_cors(&[TENANT], &selected, |_, _| None).expect("configured");
        assert!(!effective.enabled);

        // An origin the configuration *does* name gets no CORS header: the
        // browser's same-origin policy is what blocks it, not the gateway.
        let allowed = CorsRequest {
            origin: Some("https://app.example.com".to_owned()),
            method: "GET".to_owned(),
        };
        let headers = cors_response_headers(&effective, &allowed).expect("nothing is enforced");
        assert!(
            headers.is_empty(),
            "a disabled configuration vouches for no origin, not even a listed one"
        );

        // And a foreign origin is not rejected either: nothing is policed, so
        // no 403 `cf.oagw.cors.origin_not_allowed.v1` is raised.
        let foreign = CorsRequest {
            origin: Some("https://evil.com".to_owned()),
            method: "GET".to_owned(),
        };
        assert!(
            cors_response_headers(&effective, &foreign)
                .expect("a disabled configuration rejects nothing")
                .is_empty()
        );
    }

    #[test]
    fn the_wildcard_origin_matches_everything() {
        let wildcard = config(SharingMode::Private, &["*"], false);

        assert!(origin_allowed(&wildcard, "https://anything.example.com"));
        assert!(origin_allowed(&wildcard, "https://app.example.com"));
    }

    #[test]
    fn origin_matching_is_exact_and_case_sensitive() {
        let config = config(SharingMode::Private, &["https://app.example.com"], false);

        assert!(origin_allowed(&config, "https://app.example.com"));
        assert!(
            !origin_allowed(&config, "https://app.example.com:8443"),
            "port-sensitive"
        );
        assert!(
            !origin_allowed(&config, "http://app.example.com"),
            "protocol-sensitive"
        );
        assert!(!origin_allowed(&config, "https://evil.com"));
        assert!(
            !origin_allowed(&config, "https://evil.com.example.com"),
            "a suffix is not a match: no pattern matching (ADR-0004)"
        );
    }

    #[test]
    fn method_matching_is_case_insensitive_and_configured_only() {
        let config = config(SharingMode::Private, &[], false);

        assert!(method_allowed(&config, "GET"));
        assert!(method_allowed(&config, "get"));
        assert!(!method_allowed(&config, "DELETE"));
        assert!(!method_allowed(&config, ""));
    }

    #[test]
    fn an_allowed_origin_gets_the_cors_response_headers() {
        let config = CorsConfig {
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: true,
            ..config(
                SharingMode::Private,
                &["https://app.example.com", "https://admin.example.com"],
                true,
            )
        };
        let request = CorsRequest {
            origin: Some("https://app.example.com".to_owned()),
            method: "POST".to_owned(),
        };

        let headers = cors_response_headers(&config, &request).expect("allowed");

        assert_eq!(
            header_str(&headers, ALLOW_ORIGIN),
            Some("https://app.example.com")
        );
        assert_eq!(header_str(&headers, EXPOSE_HEADERS), Some("X-Request-ID"));
        assert_eq!(header_str(&headers, ALLOW_CREDENTIALS), Some("true"));
        assert_eq!(header_str(&headers, VARY), Some("Origin"));
    }

    #[test]
    fn a_wildcard_configuration_still_echoes_the_request_origin() {
        let config = config(SharingMode::Private, &["*"], false);
        let request = CorsRequest {
            origin: Some("https://app.example.com".to_owned()),
            method: "GET".to_owned(),
        };

        let headers = cors_response_headers(&config, &request).expect("allowed");

        assert_eq!(
            header_str(&headers, ALLOW_ORIGIN),
            Some("https://app.example.com")
        );
        assert!(header_str(&headers, ALLOW_CREDENTIALS).is_none());
    }

    #[test]
    fn a_disallowed_origin_is_a_403_problem() {
        let config = config(SharingMode::Private, &["https://app.example.com"], false);
        let request = CorsRequest {
            origin: Some("https://evil.com".to_owned()),
            method: "GET".to_owned(),
        };

        let error = cors_response_headers(&config, &request).expect_err("disallowed origin");

        assert_eq!(error.status().as_u16(), 403);
        assert_eq!(error.kind(), OagwErrorKind::CorsOriginNotAllowed);
        assert_eq!(
            error.kind().gts_type_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert!(error.detail().contains("https://evil.com"));
        assert_eq!(
            error
                .extensions()
                .get("origin")
                .and_then(|value| value.as_str()),
            Some("https://evil.com")
        );
    }

    #[test]
    fn a_disallowed_method_is_a_403_problem() {
        let config = config(SharingMode::Private, &["https://app.example.com"], false);
        let request = CorsRequest {
            origin: Some("https://app.example.com".to_owned()),
            method: "DELETE".to_owned(),
        };

        let error = cors_response_headers(&config, &request).expect_err("disallowed method");

        assert_eq!(error.status().as_u16(), 403);
        assert_eq!(error.kind(), OagwErrorKind::CorsMethodNotAllowed);
        assert_eq!(
            error.kind().gts_type_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
        assert!(error.detail().contains("DELETE"));
    }

    #[test]
    fn a_request_without_an_origin_is_not_a_cors_request() {
        let config = config(SharingMode::Private, &[], false);
        let request = CorsRequest {
            origin: None,
            method: "GET".to_owned(),
        };

        let headers = cors_response_headers(&config, &request).expect("no origin, no check");

        assert!(
            headers.is_empty(),
            "no CORS headers for a same-origin request"
        );
    }
}
