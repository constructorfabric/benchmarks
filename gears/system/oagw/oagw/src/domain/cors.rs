//! CORS (ADR-0004): the built-in CORS handler's decision logic.
//!
//! The functions here are pure: they take an origin, a method and the
//! effective configuration and return either a decision or the exact response
//! headers to emit. No axum router or service state is involved, so slice 4
//! maps the returned [`PreflightResponse`] / header pairs onto its response
//! parts directly.
//!
//! ## Security posture (ADR-0004)
//!
//! * CORS is off unless `enabled` is set;
//! * origin matching is an exact, byte-for-byte comparison — no patterns, and
//!   scheme/host case is *not* normalised, so an operator must list origins
//!   exactly as browsers send them;
//! * matching is port- and protocol-sensitive (`https://a.example.com:443` is
//!   a different origin from `http://a.example.com` and from
//!   `https://a.example.com:8443`);
//! * `*` matches every origin but cannot be combined with
//!   `allow_credentials`;
//! * `Vary: Origin` is emitted on every CORS-affected response.
//!
//! ## Preflight
//!
//! A preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered locally with a permissive `204` that *echoes* the requested
//! origin, method and headers; origin and method enforcement happens on the
//! subsequent actual request. Preflight requests bypass per-request auth and
//! plugin checks but remain subject to infrastructure-level controls. A
//! preflight no configured upstream owns — the caller is unauthenticated, the
//! alias does not resolve — gets [`unresolved_preflight`]: the same `204`, but
//! without an `Access-Control-Allow-Origin`, because no configuration was
//! available to verify a grant against.
//!
//! ## Inheritance
//!
//! [`merge_cors`] walks an ancestor → descendant chain keyed on the *outer*
//! entry's `sharing` mode: `private` hides the parent from descendants,
//! `inherit` unions the origin (and header) sets, `enforce` keeps the parent's
//! set and drops the child's additions. Because the union of two individually
//! valid drafts is not itself validated, the merged configuration is checked
//! against the credentials rule again: a wildcard origin in the merged set
//! always disables `allow_credentials`.

use axum::http::{HeaderName, HeaderValue};

use crate::domain::error::OagwError;
use crate::domain::model::{CorsConfig, CorsMethod, SharingMode};
use crate::domain::plugin::cors_error;

/// `Access-Control-Max-Age` the ADR-0004 pins the preflight response to.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// `Access-Control-Allow-Origin` header name.
pub const ALLOW_ORIGIN_HEADER: &str = "access-control-allow-origin";
/// `Access-Control-Allow-Methods` header name.
pub const ALLOW_METHODS_HEADER: &str = "access-control-allow-methods";
/// `Access-Control-Allow-Headers` header name.
pub const ALLOW_HEADERS_HEADER: &str = "access-control-allow-headers";
/// `Access-Control-Allow-Credentials` header name.
pub const ALLOW_CREDENTIALS_HEADER: &str = "access-control-allow-credentials";
/// `Access-Control-Expose-Headers` header name.
pub const EXPOSE_HEADERS_HEADER: &str = "access-control-expose-headers";
/// `Access-Control-Max-Age` header name.
pub const MAX_AGE_HEADER: &str = "access-control-max-age";
/// `Vary` header name.
pub const VARY_HEADER: &str = "vary";
/// Inbound `Origin` header name.
pub const ORIGIN_HEADER: &str = "origin";
/// Inbound `Access-Control-Request-Method` header name.
pub const REQUEST_METHOD_HEADER: &str = "access-control-request-method";
/// Inbound `Access-Control-Request-Headers` header name.
pub const REQUEST_HEADERS_HEADER: &str = "access-control-request-headers";

/// Wildcard origin: matches every origin, never with credentials.
pub const WILDCARD_ORIGIN: &str = "*";

/// `Vary` value of a preflight response (ADR-0004).
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// `Vary` value of an actual CORS response (ADR-0004).
pub const ACTUAL_VARY: &str = "Origin";

/// GTS error type id of a rejected origin (ADR-0004).
pub const CORS_ORIGIN_NOT_ALLOWED_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";

/// GTS error type id of a rejected method (ADR-0004).
pub const CORS_METHOD_NOT_ALLOWED_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// Default `allowed_methods` when the configuration omits the field (the
/// ADR-0004 schema default).
const DEFAULT_ALLOWED_METHODS: [&str; 2] = ["GET", "POST"];

/// Fully resolved CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectiveCorsConfig {
    /// Master switch; `false` disables every CORS response header.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    pub allowed_origins: Vec<String>,
    /// Allowed methods, upper-case.
    pub allowed_methods: Vec<String>,
    /// Headers accepted on preflight (echoed when the browser asks for others).
    pub allow_headers: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    pub expose_headers: Vec<String>,
    /// Allow credentials; requires non-wildcard origins.
    pub allow_credentials: bool,
    /// `Access-Control-Max-Age` override in seconds.
    pub max_age: Option<u64>,
}

impl EffectiveCorsConfig {
    /// `true` when `origin` is allowed.
    ///
    /// The comparison trims surrounding ASCII whitespace and is otherwise an
    /// exact byte match (see the module docs).
    #[must_use]
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        let candidate = origin.trim();
        if candidate.is_empty() {
            return false;
        }
        self.allowed_origins
            .iter()
            .any(|allowed| allowed.trim() == candidate || allowed.trim() == WILDCARD_ORIGIN)
    }

    /// `true` when `method` is allowed (case-insensitive on the method name).
    #[must_use]
    pub fn is_method_allowed(&self, method: &str) -> bool {
        let candidate = method.trim().to_ascii_uppercase();
        !candidate.is_empty() && self.allowed_methods.contains(&candidate)
    }

    /// `true` when the configuration allows every origin.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed.trim() == WILDCARD_ORIGIN)
    }

    /// `Access-Control-Max-Age` value, defaulting to the ADR-0004 constant.
    #[must_use]
    pub fn max_age(&self) -> String {
        self.max_age
            .map(|seconds| seconds.to_string())
            .unwrap_or_else(|| PREFLIGHT_MAX_AGE.to_owned())
    }
}

impl From<&CorsConfig> for EffectiveCorsConfig {
    fn from(config: &CorsConfig) -> Self {
        let mut allowed_methods: Vec<String> = config
            .allowed_methods
            .iter()
            .map(|method| cors_method_name(method).to_owned())
            .collect();
        if allowed_methods.is_empty() {
            allowed_methods = DEFAULT_ALLOWED_METHODS
                .iter()
                .map(|m| (*m).to_owned())
                .collect();
        }
        Self {
            enabled: config.enabled,
            allowed_origins: config.allowed_origins.clone(),
            allowed_methods,
            allow_headers: config.allow_headers.clone(),
            expose_headers: config.expose_headers.clone(),
            allow_credentials: config.allow_credentials,
            max_age: config.max_age,
        }
    }
}

/// Wire spelling of a configured CORS method.
#[must_use]
pub const fn cors_method_name(method: &CorsMethod) -> &'static str {
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

/// Rejects a configuration that combines `allow_credentials` with the wildcard
/// origin (ADR-0004 security restriction).
///
/// # Errors
///
/// Returns [`OagwError::validation`] naming the offending combination.
pub fn validate_cors_config(config: &CorsConfig) -> Result<(), OagwError> {
    if config.allow_credentials
        && config
            .allowed_origins
            .iter()
            .any(|origin| origin.trim() == WILDCARD_ORIGIN)
    {
        return Err(
            OagwError::validation("cannot use allow_credentials with wildcard origin")
                .with_invalid_value(WILDCARD_ORIGIN),
        );
    }
    Ok(())
}

/// Resolves the effective CORS configuration over an ancestor → descendant
/// chain, keyed on the *outer* entry's sharing mode.
///
/// Entries with `enabled == false` contribute nothing: a disabled hop is
/// invisible to its descendants rather than widening or narrowing them.
///
/// The result is re-checked against the ADR-0004 credentials rule
/// ([`enforce_no_credentials_with_wildcard`]): a merged configuration is
/// assembled from drafts that each validated on their own, so the union it
/// forms is never itself passed through [`validate_cors_config`].
///
/// Returns `None` when no entry contributes a configuration.
#[must_use]
pub fn merge_cors(chain: &[&CorsConfig]) -> Option<EffectiveCorsConfig> {
    let mut effective: Option<EffectiveCorsConfig> = None;
    let mut previous_sharing: Option<SharingMode> = None;
    for config in chain {
        if !config.enabled {
            continue;
        }
        let own = EffectiveCorsConfig::from(*config);
        effective = Some(match (effective, previous_sharing) {
            (None, _) => own,
            (Some(_parent), Some(SharingMode::Private)) => own,
            (Some(parent), Some(SharingMode::Inherit)) => union(&parent, &own),
            (Some(parent), Some(SharingMode::Enforce)) => parent,
            (Some(parent), None) => parent,
        });
        previous_sharing = Some(config.sharing);
    }
    effective.map(enforce_no_credentials_with_wildcard)
}

/// Applies the ADR-0004 rule "cannot use `allow_credentials` with wildcard
/// origin" to an already-assembled effective configuration.
///
/// [`validate_cors_config`] enforces the rule on a *single* draft, at write
/// time. A merged configuration is built from two such drafts, and the union of
/// two individually valid drafts can be invalid together:
///
/// ```text
/// upstream: { sharing: "inherit", allowed_origins: ["*"],           allow_credentials: false }
/// route:    {                        allowed_origins: ["https://app.example.com"], allow_credentials: true }
/// merged:   allowed_origins: ["*", "https://app.example.com"],      allow_credentials: true
/// ```
///
/// Merged configurations are never re-validated, so the invariant is enforced
/// here, where the merge happens. **The wildcard wins and the credential grant
/// is dropped**: `actual_response_headers` then answers the literal `*` — which
/// a browser refuses to pair with credentials — and never emits
/// `Access-Control-Allow-Credentials`, so no response can carry an echoed
/// (attacker-chosen) origin next to a credential grant. Dropping the wildcard
/// instead would keep the credentials but silently lock out every origin the
/// wildcard already serves, which is the wider regression.
fn enforce_no_credentials_with_wildcard(mut config: EffectiveCorsConfig) -> EffectiveCorsConfig {
    if config.is_wildcard() && config.allow_credentials {
        config.allow_credentials = false;
    }
    config
}

fn union(parent: &EffectiveCorsConfig, child: &EffectiveCorsConfig) -> EffectiveCorsConfig {
    EffectiveCorsConfig {
        enabled: child.enabled || parent.enabled,
        allowed_origins: union_lists(&parent.allowed_origins, &child.allowed_origins),
        allowed_methods: union_lists(&parent.allowed_methods, &child.allowed_methods),
        allow_headers: union_lists(&parent.allow_headers, &child.allow_headers),
        expose_headers: union_lists(&parent.expose_headers, &child.expose_headers),
        allow_credentials: parent.allow_credentials || child.allow_credentials,
        max_age: child.max_age.or(parent.max_age),
    }
}

fn union_lists(parent: &[String], child: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(parent.len() + child.len());
    for value in parent.iter().chain(child.iter()) {
        if !merged.contains(value) {
            merged.push(value.clone());
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// A locally answered CORS preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightResponse {
    /// HTTP status (`204 No Content`).
    pub status: u16,
    /// Response headers, in emission order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// `true` when the preflight grants the requested cross-origin access.
    ///
    /// The response stays permissive either way (ADR-0004 echoes the request),
    /// so this flag — not the presence of an allow header — is the verdict a
    /// caller should log or surface.
    pub allowed: bool,
}

impl PreflightResponse {
    /// `true` when the preflight grants the requested cross-origin access.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    /// Value of a single header, for assertions and for the proxy handler.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.as_str() == name)
            .map(|(_, value)| value.to_str().unwrap_or_default())
    }
}

/// Evaluates a CORS preflight request (ADR-0004 "Preflight Request Handling").
///
/// The response is permissive: the requested origin, method and headers are
/// echoed back and enforcement is deferred to the actual request. When CORS is
/// disabled (or the request carries no `Origin`) only the `Vary` triplet is
/// returned, so a misrouted preflight never advertises a cross-origin grant.
#[must_use]
pub fn evaluate_preflight(
    config: &EffectiveCorsConfig,
    origin: Option<&str>,
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> PreflightResponse {
    let mut headers = vec![vary_header(PREFLIGHT_VARY)];
    let granted = config.enabled
        && origin
            .map(str::trim)
            .is_some_and(|origin| !origin.is_empty() && config.is_origin_allowed(origin));
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return PreflightResponse {
            status: 204,
            headers,
            allowed: false,
        };
    };
    if !config.enabled {
        return PreflightResponse {
            status: 204,
            headers,
            allowed: false,
        };
    }

    headers.push(header(ALLOW_ORIGIN_HEADER, origin));
    let methods = request_method
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_uppercase())
        .unwrap_or_else(|| config.allowed_methods.join(", "));
    headers.push(header(ALLOW_METHODS_HEADER, &methods));
    let requested_headers = request_headers
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| config.allow_headers.join(", "));
    headers.push(header(ALLOW_HEADERS_HEADER, &requested_headers));
    if config.allow_credentials {
        headers.push(header(ALLOW_CREDENTIALS_HEADER, "true"));
    }
    headers.push(header(MAX_AGE_HEADER, &config.max_age()));
    PreflightResponse {
        status: 204,
        headers,
        allowed: granted,
    }
}

/// `true` when the inbound request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &str, origin: Option<&str>, request_method: Option<&str>) -> bool {
    method.eq_ignore_ascii_case("OPTIONS")
        && origin.is_some_and(|value| !value.trim().is_empty())
        && request_method.is_some_and(|value| !value.trim().is_empty())
}

/// `Access-Control-Allow-Methods` of a preflight the gateway cannot bind to a
/// configured upstream: every method the proxy surface forwards, so the browser
/// retries the actual request and the gateway enforces the origin there.
pub const FALLBACK_ALLOWED_METHODS: &str = "GET, HEAD, POST, PUT, PATCH, DELETE";

/// `Access-Control-Allow-Headers` of such a preflight when the browser named
/// none, i.e. the headers a cross-origin caller needs for an authenticated
/// gateway request.
pub const FALLBACK_ALLOW_HEADERS: &str = "authorization, content-type, x-request-id";

/// Answers a preflight that cannot be bound to a configured upstream
/// (ADR-0004 "Preflight Request Handling").
///
/// A browser preflight carries no credentials, so the gateway may have no
/// tenant context and must still answer `204` locally instead of failing it
/// with `401` or `404`. No CORS configuration is available in that case, so
/// the answer echoes the requested method and headers, pins the ADR-0004
/// max-age and carries the `Vary` triplet — but **no**
/// `Access-Control-Allow-Origin`: a grant the gateway could not verify against
/// a configuration is never advertised, and the browser therefore blocks the
/// cross-origin read. Enforcement still happens on the actual request.
#[must_use]
pub fn unresolved_preflight(
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> PreflightResponse {
    let mut headers = vec![vary_header(PREFLIGHT_VARY)];
    let methods = request_method
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_uppercase())
        .unwrap_or_else(|| FALLBACK_ALLOWED_METHODS.to_owned());
    headers.push(header(ALLOW_METHODS_HEADER, &methods));
    let requested = request_headers
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| FALLBACK_ALLOW_HEADERS.to_owned());
    headers.push(header(ALLOW_HEADERS_HEADER, &requested));
    headers.push(header(MAX_AGE_HEADER, PREFLIGHT_MAX_AGE));
    PreflightResponse {
        status: 204,
        headers,
        allowed: false,
    }
}

// ---------------------------------------------------------------------------
// Actual requests
// ---------------------------------------------------------------------------

/// Validates an actual cross-origin request (ADR-0004 "Actual Request
/// Handling").
///
/// Same-origin requests (no `Origin` header) and requests to a CORS-disabled
/// upstream are never rejected here: CORS only governs cross-origin access.
///
/// # Errors
///
/// Returns a `403` problem document with the ADR-0004 GTS error id when the
/// origin or the method is not allowed.
pub fn validate_actual_request(
    config: &EffectiveCorsConfig,
    origin: Option<&str>,
    method: &str,
) -> Result<(), OagwError> {
    if !config.enabled {
        return Ok(());
    }
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    if !config.is_origin_allowed(origin) {
        return Err(cors_error(
            CORS_ORIGIN_NOT_ALLOWED_TYPE,
            "CORS Origin Not Allowed",
            format!("Origin '{origin}' not in allowed origins list"),
            origin.to_owned(),
        ));
    }
    if !config.is_method_allowed(method) {
        return Err(cors_error(
            CORS_METHOD_NOT_ALLOWED_TYPE,
            "CORS Method Not Allowed",
            format!("Method '{}' not in allowed methods list", method.trim()),
            method.trim().to_owned(),
        ));
    }
    Ok(())
}

/// Response headers an actual CORS request contributes to the upstream
/// response.
///
/// Returns the `Vary: Origin` header plus the allow/expose set. The list is
/// empty when CORS is disabled or when the origin is not allowed: a rejected
/// request never reaches the upstream (its `403` carries its own headers) and
/// a same-origin request gets no CORS headers.
#[must_use]
pub fn actual_response_headers(
    config: &EffectiveCorsConfig,
    origin: Option<&str>,
) -> Vec<(HeaderName, HeaderValue)> {
    if !config.enabled {
        return Vec::new();
    }
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return vec![vary_header(ACTUAL_VARY)];
    };
    if !config.is_origin_allowed(origin) {
        return Vec::new();
    }
    let mut headers = vec![vary_header(ACTUAL_VARY)];
    let allow_origin = if config.is_wildcard() && !config.allow_credentials {
        WILDCARD_ORIGIN.to_owned()
    } else {
        origin.to_owned()
    };
    headers.push(header(ALLOW_ORIGIN_HEADER, &allow_origin));
    if config.allow_credentials {
        headers.push(header(ALLOW_CREDENTIALS_HEADER, "true"));
    }
    if !config.expose_headers.is_empty() {
        headers.push(header(
            EXPOSE_HEADERS_HEADER,
            &config.expose_headers.join(", "),
        ));
    }
    headers
}

fn vary_header(value: &str) -> (HeaderName, HeaderValue) {
    header(VARY_HEADER, value)
}

fn header(name: &'static str, value: &str) -> (HeaderName, HeaderValue) {
    let header_name = HeaderName::from_static(name);
    let header_value =
        HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("*"));
    (header_name, header_value)
}

#[cfg(test)]
#[path = "cors_tests.rs"]
mod tests;
