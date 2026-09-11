//! The built-in CORS handler of the Data Plane (FEATURE entry 2.8,
//! `cpt-cf-oagw-adr-cors`).
//!
//! CORS is **not** a guard plugin (`cpt-cf-oagw-adr-cors`): it is a first-class
//! field on `Upstream.cors` / `Route.cors` handled by core Data Plane logic,
//! and the `cors` guard identifier stays catalog-only. Everything in this
//! module is a pure function of the effective [`CorsConfig`], the request
//! method and the request `Origin`; nothing here resolves an alias, selects an
//! endpoint, executes a plugin, contacts an upstream, or holds a cache
//! (`cpt-cf-oagw-dod-cors-preflight`,
//! `cpt-cf-oagw-dod-cors-origin-enforcement`).
//!
//! # The two positions of the canonical proxy-path order
//!
//! * **preflight** — an `OPTIONS` carrying `Origin` and
//!   `Access-Control-Request-Method` is answered locally with a permissive
//!   `204`, echoed within the reflection bounds of
//!   [`preflight_response_headers`], and is **never** forwarded: the answer
//!   grants nothing on its own, because the actual request is re-checked here
//!   (`cpt-cf-oagw-flow-cors-preflight`).
//! * **actual request** — a request carrying an `Origin` is a CORS subject:
//!   the origin is matched first ([`match_origin`]) and the method second, a
//!   disallowed origin is never reported as a method failure
//!   ([`evaluate`]), and an allowed request carries the four response headers
//!   of [`response_header_pairs`] (`cpt-cf-oagw-flow-cors-actual-request`).
//!
//! Deny-by-default governs actual requests: no `cors` block, or `enabled:
//! false`, applies no check, adds no `Access-Control-*` header and still emits
//! `Vary: Origin`, so the cross-origin read is denied by the browser rather
//! than by a `403` (`cpt-cf-oagw-dod-cors-deny-by-default`).
// @cpt-algo:cpt-cf-oagw-algo-cors-config-validate:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-config-validation:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-hierarchical-origins:p1
// @cpt-dod:cpt-cf-oagw-dod-cors-origin-matching:p1

use crate::domain::dto::CorsConfig;
use crate::domain::error::DomainError;

// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-1
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-2
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-3
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-4
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-5
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-6
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-7
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-8
// @cpt-begin:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-9
/// The documented CORS method set of `docs/schemas/upstream.v1.schema.json`
/// (`cpt-cf-oagw-algo-cors-config-validate` step 4). The singular source of
/// the set: the configuration validation, the preflight reflection and the
/// actual-request method check all read it.
pub const CORS_METHODS: [&str; 7] =
//
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-9
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-8
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-7
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-6
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-5
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-4
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-3
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-2
// @cpt-end:cpt-cf-oagw-algo-cors-config-validate:p1:inst-cors-alg-cv-1
//
    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// The `Access-Control-Max-Age` of the permissive preflight answer
/// (`inst-cors-alg-pf-4`).
pub const MAX_AGE: &str = "86400";

/// The byte bound of a reflected preflight value
/// (`cpt-cf-oagw-algo-cors-preflight-response` step 3). A value over the bound
/// is dropped and the header is omitted; the bound is total, so it also bounds
/// every comma-separated header name inside the value.
pub const REFLECTION_BOUND_BYTES: usize = 4096;

/// The three-part `Vary` a preflight answer carries
/// (`inst-cors-alg-pf-5`): no shared cache may reuse one preflight answer for
/// another.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// The `Vary` member every CORS-relevant response carries
/// (`cpt-cf-oagw-algo-cors-response-headers` step 4).
pub const VARY_ORIGIN: &str = "Origin";

/// The `Vary` header name, compared exactly as the framework lowercased it.
const VARY_HEADER: &str = "vary";

/// Whether `method` is in the documented CORS method set.
#[must_use]
pub fn is_legal_method(method: &str) -> bool {
    CORS_METHODS.contains(&method)
}

/// Whether `value` may be reflected into a response header
/// (`cpt-cf-oagw-algo-cors-preflight-response` step 3): no CR, LF, NUL or other
/// control character, and at most [`REFLECTION_BOUND_BYTES`] bytes. A single
/// reflected name cannot exceed the value it is part of, so the total bound is
/// the one check a comma-joined list needs.
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-3
// `inst-cors-alg-pf-3`: the reflection bounds — no control character and no
// value over 4096 bytes; a failing value is dropped and the header omitted
// rather than answered with a rejection, because preflight reflection is not a
// policy grant.
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-2
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-4
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-5
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-6
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-7
#[must_use]
pub fn reflection_allowed(value: &str) -> bool {
    value.len() <= REFLECTION_BOUND_BYTES && !value.chars().any(char::is_control)
}
//
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-7
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-6
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-5
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-4
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-2
//
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-3

/// The canonical origin form both sides of a comparison serialize to
/// (`cpt-cf-oagw-algo-cors-origin-match` step 1): `scheme://host[:port]`, with
/// the scheme and the host lowercased and the port omitted when it equals the
/// scheme default — `443` for `https`, `80` for `http`.
///
/// Returns `None` for a value that is not a `scheme://host` origin, which can
/// never match a configured entry.
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-1
// `inst-cors-alg-om-1`: one serialization, applied to the request origin and
// to every configured entry, so `https://app.example.com:443` and
// `https://app.example.com` compare equal while `:8443` never does.
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-3
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-4
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-5
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-6
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-7
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-8
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-9
#[must_use]
pub fn canonical_origin(origin: &str) -> Option<String> {
    let parsed = url::Url::parse(origin.trim()).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    };
    let port = parsed.port();
    Some(match port {
        Some(port) if Some(port) != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    })
}
//
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-9
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-8
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-7
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-6
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-5
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-4
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-3
//
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-1

/// The verdict of an origin comparison
/// (`cpt-cf-oagw-algo-cors-origin-match`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginVerdict {
    /// A configured entry equals the request origin exactly; the entry is the
    /// `Access-Control-Allow-Origin` value, and the only verdict a credentialed
    /// policy may carry.
    Exact(String),
    /// No entry equals the request origin and the effective set carries `*`,
    /// which is legal only while `allow_credentials` is false.
    Wildcard,
    /// No entry equals the request origin and no wildcard is present: the
    /// `403` origin rejection.
    NoMatch,
}

/// Match a request origin against the effective `allowed_origins` set
/// (`cpt-cf-oagw-algo-cors-origin-match`).
///
/// The comparison is exact across scheme, host and port after both sides are
/// serialized to the canonical form, port-sensitive and protocol-sensitive,
/// with **no** regex, wildcard-host, suffix or registrable-domain relaxation.
/// An exact entry always wins over a `*` wildcard when both are present.
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-2
// `inst-cors-alg-om-2` .. `-9`: the constant-time set comparison — every entry
// is serialized and compared for equality, `*` is the only relaxation, and an
// exact match returns before the wildcard is ever considered.
#[must_use]
pub fn match_origin(origin: &str, allowed: &[String]) -> OriginVerdict {
    let Some(requested) = canonical_origin(origin) else {
        return OriginVerdict::NoMatch;
    };
    let mut wildcard = false;
    for entry in allowed {
        if entry == "*" {
            wildcard = true;
            continue;
        }
        if canonical_origin(entry).is_some_and(|canonical| canonical == requested) {
            // The configured entry, serialized to the same canonical form the
            // request origin was, is what the response echoes.
            return OriginVerdict::Exact(canonical_origin(entry).expect("just serialized"));
        }
    }
    if wildcard {
        OriginVerdict::Wildcard
    } else {
        OriginVerdict::NoMatch
    }
}
// @cpt-end:cpt-cf-oagw-algo-cors-origin-match:p1:inst-cors-alg-om-2

/// The CORS inputs the proxy handler's preflight call-in delivers to the
/// responder (`cpt-cf-oagw-algo-cors-preflight-response` input): the effective
/// `allow_credentials` and whether the origin-match verdict is `exact`.
///
/// A preflight resolves no tenant, no route and no endpoint, so the inputs are
/// delivered when the addressed alias's own `cors` block was consulted and
/// `None` when it was not — in which case the answer stays permissive and
/// credential-free, because a preflight grants nothing on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreflightCors {
    /// The effective `allow_credentials`.
    pub allow_credentials: bool,
    /// Whether the matched verdict is `exact`.
    pub exact: bool,
}

/// The preflight header set
/// (`cpt-cf-oagw-algo-cors-preflight-response`).
///
/// The origin is copied verbatim and never validated, because enforcement is
/// deferred to the actual request; the requested method is copied only when it
/// is in the legal method set; the requested headers are copied only within the
/// reflection bounds; `Access-Control-Allow-Credentials` is emitted only for an
/// exact match under a credentialed policy; and
/// `Access-Control-Expose-Headers` is never emitted on a preflight.
// @cpt-begin:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-1
// `inst-cors-alg-pf-1` .. `-7`: the `204` header set — the reflected origin,
// the reflected method inside the legal set, the reflected headers inside the
// reflection bounds, the one-day max-age, the three-part `Vary`, and the
// credentials header only for an exact match under a credentialed policy.
#[must_use]
pub fn preflight_response_headers(
    origin: Option<&str>,
    requested_method: Option<&str>,
    requested_headers: Option<&str>,
    cors: Option<PreflightCors>,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(origin) = origin.filter(|value| reflection_allowed(value)) {
        headers.push(("access-control-allow-origin".to_owned(), origin.to_owned()));
    }
    if let Some(method) = requested_method.filter(|method| is_legal_method(method)) {
        headers.push(("access-control-allow-methods".to_owned(), method.to_owned()));
    }
    if let Some(requested) = requested_headers.filter(|value| reflection_allowed(value)) {
        headers.push(("access-control-allow-headers".to_owned(), requested.to_owned()));
    }
    headers.push(("access-control-max-age".to_owned(), MAX_AGE.to_owned()));
    headers.push((VARY_HEADER.to_owned(), PREFLIGHT_VARY.to_owned()));
    if cors.is_some_and(|cors| cors.allow_credentials && cors.exact) {
        headers.push(("access-control-allow-credentials".to_owned(), "true".to_owned()));
    }
    headers
}
// @cpt-end:cpt-cf-oagw-algo-cors-preflight-response:p1:inst-cors-alg-pf-1

/// The outcome label this feature supplies to the shared observability record
/// (`cpt-cf-oagw-feature-observability-and-operability`); it mints no metric
/// name and emits no record of its own (`cors.md` §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorsOutcome {
    /// A preflight was answered locally, with no upstream round trip.
    PreflightShortCircuit,
    /// An actual request carried a disallowed origin.
    OriginNotAllowed,
    /// An actual request carried a disallowed method, after the origin passed.
    MethodNotAllowed,
    /// A merged effective configuration combined credentials with a wildcard
    /// origin and was fail-closed rather than served.
    MergedConfigRejected,
}

impl CorsOutcome {
    /// The label the shared observability record carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreflightShortCircuit => "preflight_short_circuit",
            Self::OriginNotAllowed => "origin_not_allowed",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::MergedConfigRejected => "merged_config_rejected",
        }
    }
}

/// The pipeline-boundary CORS outcome entry 2.9 consumes
/// (`cpt-cf-oagw-dod-cors-unit-tests`, `cors.md` §1.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsObservation {
    /// The outcome label, and nothing else.
    pub outcome: CorsOutcome,
}

/// What the actual-request CORS evaluation decided
/// (`cpt-cf-oagw-algo-cors-request-evaluation`).
#[derive(Debug, Clone, PartialEq)]
pub struct CorsEvaluation {
    /// The outcome label of the evaluation, and `None` when the request
    /// produced no CORS outcome of its own: a request without an `Origin` is
    /// not a CORS subject, and a forwarded request under a disabled
    /// configuration is denied by the browser rather than by the gateway.
    pub outcome: Option<CorsOutcome>,
    /// The `Access-Control-*` response header pairs to add to whatever response
    /// the exchange produces, empty on a rejection — the shared error contract
    /// renders `Vary: Origin` on a CORS rejection itself.
    pub headers: Vec<(String, String)>,
    /// Whether `Vary: Origin` is appended to the response the exchange
    /// produces (`cpt-cf-oagw-algo-cors-response-headers` step 4): every
    /// request that carries an `Origin` is CORS-relevant, and a request without
    /// one is not.
    pub vary: bool,
    /// The `403` gateway rejection, and `None` when the request is forwarded.
    pub rejection: Option<DomainError>,
}

impl CorsEvaluation {
    /// A forwarded request that is not a CORS subject: no check, no header.
    #[must_use]
    fn not_a_cors_subject() -> Self {
        Self { outcome: None, headers: Vec::new(), vary: false, rejection: None }
    }

    /// A forwarded request that carries an `Origin` and is answered by the
    /// browser's same-origin policy instead of by a `403`.
    #[must_use]
    fn forwarded_without_cors_headers() -> Self {
        Self { outcome: None, headers: Vec::new(), vary: true, rejection: None }
    }
}

/// Whether the effective configuration is serveable
/// (`cpt-cf-oagw-algo-cors-config-validate` step 7, re-applied to a merged
/// configuration): `allow_credentials: true` combined with a `*` origin is
/// never served. A merged result that combines the two is fail-closed at
/// request time, and no merged configuration is ever persisted, so the stored
/// records are unchanged by the rejection.
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-5
// `inst-cors-hier-5`/`-6`: the credentials-and-wildcard rule is re-applied to
// the merged effective configuration, so an inherited wildcard can never be
// turned into a credential-bearing policy by a descendant.
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-1
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-2
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-3
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-3-1
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-4
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-4-1
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-6
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-6-1
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-7
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-8
// @cpt-begin:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-8-1
#[must_use]
pub fn serveable(cors: &CorsConfig) -> bool {
    !(cors.allow_credentials
        && cors.allowed_origins.iter().flatten().any(|origin| origin == "*"))
}
//
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-8-1
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-8
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-7
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-6-1
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-6
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-4-1
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-4
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-3-1
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-3
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-2
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-1
//
// @cpt-end:cpt-cf-oagw-flow-cors-hierarchical-origins:p1:inst-cors-hier-5

/// Evaluate an actual request against the effective CORS configuration
/// (`cpt-cf-oagw-algo-cors-request-evaluation`).
///
/// Only a request that carries an `Origin` is a CORS subject. The origin is
/// checked first and the method second, so a disallowed origin is never
/// reported as a method failure. A disabled or absent configuration applies no
/// check and adds no `Access-Control-*` header, but still emits `Vary: Origin`.
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-1
// `inst-cors-alg-ev-1` .. `-9`: the origin check runs first, the method check
// second, and the request is forwarded only after both pass; a request without
// an `Origin` and a request under a disabled configuration are forwarded with
// no `Access-Control-*` header at all.
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-2
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-3
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-4
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-5
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-6
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-7
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-8
// @cpt-begin:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-9
#[must_use]
pub fn evaluate(
    cors: Option<&CorsConfig>,
    origin: Option<&str>,
    method: &str,
    path: Option<&str>,
    trace_id: Option<&str>,
) -> CorsEvaluation {
    let Some(origin) = origin.filter(|origin| !origin.is_empty()) else {
        return CorsEvaluation::not_a_cors_subject();
    };
    // Deny-by-default: no `cors` block, or `enabled: false`, forwards the
    // request with no `Access-Control-*` header while still emitting
    // `Vary: Origin` (`cpt-cf-oagw-dod-cors-deny-by-default`).
    let Some(cors) = cors.filter(|cors| cors.enabled) else {
        return CorsEvaluation::forwarded_without_cors_headers();
    };
    let rejection = |outcome, error| CorsEvaluation {
        outcome: Some(outcome),
        headers: Vec::new(),
        vary: true,
        rejection: Some(error),
    };
    // A merged configuration that combines credentials with a wildcard origin
    // is fail-closed rather than served (`inst-cors-hier-6`).
    if !serveable(cors) {
        return rejection(
            CorsOutcome::MergedConfigRejected,
            DomainError::CorsOriginNotAllowed { path: path.map(str::to_owned), trace_id: trace_id.map(str::to_owned) },
        );
    }
    let configured = cors.allowed_origins.clone().unwrap_or_default();
    let verdict = match_origin(origin, &configured);
    if verdict == OriginVerdict::NoMatch {
        return rejection(
            CorsOutcome::OriginNotAllowed,
            DomainError::CorsOriginNotAllowed { path: path.map(str::to_owned), trace_id: trace_id.map(str::to_owned) },
        );
    }
    if !cors.allowed_methods.iter().any(|allowed| allowed == method) {
        return rejection(
            CorsOutcome::MethodNotAllowed,
            DomainError::CorsMethodNotAllowed { path: path.map(str::to_owned), trace_id: trace_id.map(str::to_owned) },
        );
    }
    let headers = response_header_pairs(cors, &verdict);
    CorsEvaluation { outcome: None, headers, vary: true, rejection: None }
}
//
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-9
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-8
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-7
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-6
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-5
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-4
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-3
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-2
//
// @cpt-end:cpt-cf-oagw-algo-cors-request-evaluation:p1:inst-cors-alg-ev-1

/// The `Access-Control-*` response header pairs of an allowed actual request
/// (`cpt-cf-oagw-algo-cors-response-headers` steps 1 .. 3).
///
/// `Access-Control-Expose-Headers` is omitted when the effective list is empty
/// and `Access-Control-Allow-Credentials` is emitted only for an exact match
/// under a credentialed policy; `Vary: Origin` is appended to the response by
/// [`append_vary_origin`] rather than carried here, because it merges with the
/// upstream's own list.
// @cpt-begin:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-1
// `inst-cors-alg-hd-1` .. `-3`: the exact configured entry, or `*` for a
// wildcard verdict under a non-credentialed policy; the exposed headers only
// when some are configured; and the credentials header only for an exact
// match.
// @cpt-begin:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-2
// @cpt-begin:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-3
// @cpt-begin:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-5
// @cpt-begin:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-6
#[must_use]
pub fn response_header_pairs(cors: &CorsConfig, verdict: &OriginVerdict) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(allow_origin) = allow_origin_value(cors, verdict) {
        headers.push(("access-control-allow-origin".to_owned(), allow_origin));
    }
    if !cors.expose_headers.is_empty() {
        headers
            .push(("access-control-expose-headers".to_owned(), cors.expose_headers.join(", ")));
    }
    if cors.allow_credentials && matches!(verdict, OriginVerdict::Exact(_)) {
        headers.push(("access-control-allow-credentials".to_owned(), "true".to_owned()));
    }
    headers
}
//
// @cpt-end:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-6
// @cpt-end:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-5
// @cpt-end:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-3
// @cpt-end:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-2
//
// @cpt-end:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-1

/// The `Access-Control-Allow-Origin` value the verdict carries
/// (`cpt-cf-oagw-algo-cors-response-headers` step 1): the matched configured
/// entry, or `*` when the effective set is the wildcard and `allow_credentials`
/// is false.
fn allow_origin_value(cors: &CorsConfig, verdict: &OriginVerdict) -> Option<String> {
    match verdict {
        OriginVerdict::Exact(entry) => Some(entry.clone()),
        OriginVerdict::Wildcard if !cors.allow_credentials => Some("*".to_owned()),
        _ => None,
    }
}

/// The `Vary: Origin` pair an evaluation adds to a response it does not render
/// itself.
#[must_use]
pub fn vary_origin_pair() -> (String, String) {
    (VARY_HEADER.to_owned(), VARY_ORIGIN.to_owned())
}

/// Append `Origin` to the `Vary` list a response already carries
/// (`cpt-cf-oagw-algo-cors-response-headers` step 4): the upstream's list is
/// appended to rather than overwritten, and a list that already names `Origin`
/// is left as it arrived.
// @cpt-begin:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-4
// `inst-cors-alg-hd-4`: `Vary: Origin` on every CORS-relevant response, and an
// upstream `Vary` list is appended to rather than overwritten.
pub fn append_vary_origin(headers: &mut Vec<(String, String)>) {
    if headers
        .iter()
        .any(|(name, value)| name == VARY_HEADER && lists_origin(value))
    {
        return;
    }
    match headers.iter_mut().rev().find(|(name, _)| name == VARY_HEADER) {
        Some((_, value)) => {
            value.push_str(", ");
            value.push_str(VARY_ORIGIN);
        }
        None => headers.push(vary_origin_pair()),
    }
}
// @cpt-end:cpt-cf-oagw-algo-cors-response-headers:p1:inst-cors-alg-hd-4

/// Whether a `Vary` value already names `Origin` among its comma-separated
/// members.
fn lists_origin(value: &str) -> bool {
    value.split(',').any(|member| member.trim().eq_ignore_ascii_case(VARY_ORIGIN))
}

/// The field-level CORS merge of an ancestor layer and a more specific one
/// (`cpt-cf-oagw-algo-cors-origin-set-merge` step 7): the origin set is an
/// add-only union so an inherited origin can never be removed,
/// `allowed_methods` and `expose_headers` union, `enabled` takes the more
/// specific value present, and `allow_credentials` escalates monotonically —
/// `true` on any contributing layer wins and can never be turned off by a
/// descendant.
///
/// The *layer-level* semantics — a `private` ancestor contributing nothing, an
/// `enforce` ancestor standing absolutely, and the write-time override
/// permission gating a descendant addition — belong to the merge engine of
/// entry 2.1 (`cpt-cf-oagw-algo-gear-foundation-config-merge` step 8), which
/// calls this function with the two layers it already selected; the merge
/// evaluates already-stored, already-authorized configuration and performs no
/// per-request permission check
/// (`cpt-cf-oagw-dod-cors-hierarchical-origins`).
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-2
// `inst-cors-alg-mg-2` .. `-9`: the origin union is add-only and the four
// scalar rules are applied — `enabled` from the more specific layer,
// `allowed_methods` and `expose_headers` unioned, and `allow_credentials`
// escalated monotonically.
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-10
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-11
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-12
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-12-1
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-3
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-4
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-5
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-6
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-7
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-8
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-9
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-9-1
#[must_use]
pub fn merge_fields(ancestor: &CorsConfig, descendant: &CorsConfig) -> CorsConfig {
    let mut merged = ancestor.clone();
    let mut origins = ancestor.allowed_origins.clone().unwrap_or_default();
    for origin in descendant.allowed_origins.iter().flatten() {
        if !origins.contains(origin) {
            origins.push(origin.clone());
        }
    }
    merged.allowed_origins = Some(origins);
    for method in &descendant.allowed_methods {
        if !merged.allowed_methods.contains(method) {
            merged.allowed_methods.push(method.clone());
        }
    }
    for header in &descendant.expose_headers {
        if !merged.expose_headers.contains(header) {
            merged.expose_headers.push(header.clone());
        }
    }
    merged.enabled = descendant.enabled;
    merged.allow_credentials = ancestor.allow_credentials || descendant.allow_credentials;
    merged.sharing = descendant.sharing;
    merged
}
//
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-9-1
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-9
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-8
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-7
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-6
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-5
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-4
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-3
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-12-1
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-12
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-11
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-10
//
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-2

/// Store every configured origin in the canonical form the comparison
/// serializes to (`cpt-cf-oagw-algo-cors-config-validate` step 3), so a
/// configured entry and a request origin serialize to the same string.
///
/// An entry that cannot be serialized is left as it arrived: it can never match
/// a request origin, and the configuration was already validated.
pub fn canonicalize(cors: &mut CorsConfig) {
    if let Some(origins) = &mut cors.allowed_origins {
        for origin in origins.iter_mut() {
            if let Some(canonical) = canonical_origin(origin) {
                *origin = canonical;
            }
        }
    }
}
