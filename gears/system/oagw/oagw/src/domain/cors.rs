//! CORS decision of the `oagw` gear.
//!
//! Realizes `cpt-cf-oagw-dod-cors-entities`: [`CorsDecision`] and the
//! preflight response shape declared once, in the domain layer, free of
//! transport and persistence types, referencing `CorsConfig` of
//! `cpt-cf-oagw-feature-gear-foundation` and `EffectiveCors` of
//! `cpt-cf-oagw-feature-hierarchical-config` rather than redeclaring either.
//!
//! The three routines the FEATURE's CDSL §3 states are here too, because they
//! are functions over this state and over nothing else:
//! `cpt-cf-oagw-algo-cors-fold` ([`fold`]),
//! `cpt-cf-oagw-algo-cors-decide` ([`decide`]), and
//! `cpt-cf-oagw-algo-cors-preflight-headers` ([`preflight_answer`]). None of
//! them writes to storage, holds state between requests, or reaches a network.
//!
//! The two 403 answers the decision names are not [`crate::domain::error`]
//! catalogue variants — DESIGN §3.3's catalogue is closed at 22 variants, and
//! the two `type` identifiers ADR 0004 spells live in [`crate::gts`] beside
//! it and are consumed by the API layer's problem mapping.

use crate::domain::effective::EffectiveCors;
use crate::domain::upstream::{CorsConfig, SharingMode};

// @cpt-dod:cpt-cf-oagw-dod-cors-entities:p1

/// The `Access-Control-Max-Age` a preflight answer carries, which is the value
/// ADR 0004's preflight example states and the shipped `definitions.cors`
/// declares no configuration surface for (§1.5).
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// The `Vary` value a preflight answer carries, which is the three-member
/// value ADR 0004's preflight example shows.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// The `Vary` value every actual-request answer this feature decorates
/// carries, which is the always-present one ADR 0004's Security Considerations
/// state.
pub const VARY_ORIGIN: &str = "Origin";

/// The shipped schema's declared default for a method list no layer declares.
fn default_methods() -> Vec<String> {
    vec![String::from("GET"), String::from("POST")]
}

/// The effective configuration the fold produced.
///
/// `enabled` is a member of the effective configuration the CDSL §3 names, and
/// the fold never yields a policy whose prevailing `enabled` is false: such a
/// family folds to the absent outcome, which is the branch the flow reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveCorsPolicy {
    /// Whether the prevailing configuration is enabled, which is always true
    /// in a policy the fold produced.
    pub enabled: bool,
    /// The effective origins, exact-matched, `*` included.
    pub allowed_origins: Vec<String>,
    /// The effective methods, exact-matched against the schema's literals.
    pub allowed_methods: Vec<String>,
    /// The headers exposed to the browser beyond the safelisted ones.
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed on an admitted response.
    pub allow_credentials: bool,
}

/// Why an actual cross-origin request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorsRefusal {
    /// The `Origin` the request carried is not named by the effective list.
    Origin,
    /// The method the request carried is not named by the effective list.
    Method,
}

impl CorsRefusal {
    /// The problem `title` ADR 0004 gives the refusal's `type`.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Origin => "CORS Origin Not Allowed",
            Self::Method => "CORS Method Not Allowed",
        }
    }

    /// The problem `type` identifier ADR 0004 spells for the refusal.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::Origin => crate::gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            Self::Method => crate::gts::ERR_CORS_METHOD_NOT_ALLOWED,
        }
    }
}

/// The decoration an admitted actual request carries on the response the proxy
/// path assembles.
///
/// `Access-Control-Allow-Origin` carries the request's own `Origin` value and
/// never the literal `*`, so one rule covers the credentialed and the
/// non-credentialed configuration alike (§1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsDecoration {
    /// The echoed origin of `Access-Control-Allow-Origin`.
    pub allow_origin: String,
    /// Whether `Access-Control-Allow-Credentials` is emitted.
    pub allow_credentials: bool,
    /// The `Access-Control-Expose-Headers` list, empty when the header is
    /// omitted.
    pub expose_headers: Vec<String>,
    /// The `Vary` value the answer carries.
    pub vary: &'static str,
}

impl CorsDecoration {
    /// The header pairs the response carries, in emission order.
    #[must_use]
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (String::from("Access-Control-Allow-Origin"), self.allow_origin.clone()),
            (String::from("Vary"), String::from(self.vary)),
        ];
        if self.allow_credentials {
            headers.push((
                String::from("Access-Control-Allow-Credentials"),
                String::from("true"),
            ));
        }
        if !self.expose_headers.is_empty() {
            headers.push((
                String::from("Access-Control-Expose-Headers"),
                self.expose_headers.join(", "),
            ));
        }
        headers
    }
}

/// The verdict of one actual cross-origin request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsDecision {
    /// The request is admitted and the decoration rides the forwarded answer.
    Allowed(CorsDecoration),
    /// The request is refused for the reason carried, before anything is
    /// forwarded.
    Refused(CorsRefusal),
}

/// The 204 preflight answer: the status and the header set it carries.
///
/// The shape is the one ADR 0004's preflight example spells, without the
/// `Access-Control-Allow-Credentials` whose only appearance in that ADR is on
/// an actual-request response (§1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightAnswer {
    /// The status of the answer: always `204`.
    pub status: u16,
    /// The header pairs of the answer, in emission order.
    pub headers: Vec<(String, String)>,
}

/// The members one layer result declares, taken from its own object.
struct Declared {
    origins: Option<Vec<String>>,
    methods: Option<Vec<String>>,
    expose: Option<Vec<String>>,
    credentials: bool,
}

/// The members one layer result declares: a list member the object leaves
/// empty is an omission rather than a declaration, because the shipped schema
/// defaults it and a written object that omits the member deserializes to the
/// empty list (§1.5).
fn declared_of(cors: &CorsConfig) -> Declared {
    Declared {
        origins: (!cors.allowed_origins.is_empty()).then(|| cors.allowed_origins.clone()),
        methods: (!cors.allowed_methods.is_empty()).then(|| cors.allowed_methods.clone()),
        expose: (!cors.expose_headers.is_empty()).then(|| cors.expose_headers.clone()),
        credentials: cors.allow_credentials,
    }
}

// @cpt-dod:cpt-cf-oagw-dod-cors-hierarchy:p1

/// Applies the per-member overlay across the two layer results.
///
/// The union under `inherit`, the forcing under `enforce`, and the withholding
/// under `private` were applied across the ancestor chain by the hierarchical
/// feature's merge, which reported one result per layer, so this routine only
/// takes each member from the last layer result that declares it, in the
/// upstream, then route order. It never unions across layers and never
/// re-walks the chain.
///
/// Returns the absent outcome when no layer carries a `cors` object or the
/// prevailing `enabled` is false, which is the branch the enforcement flow
/// reads to enforce and decorate nothing.
#[must_use]
pub fn fold(
    upstream: Option<&EffectiveCors>,
    route: Option<&EffectiveCors>,
) -> Option<EffectiveCorsPolicy> {
    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-prevail
    // The two layer results are consumed in the upstream, then route order, so
    // the last layer result that declares a member prevails.
    let (upstream, route) = (upstream, route);
    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-enforce-if
    // An ancestor `enforce` decides the whole object: no descendant override
    // can widen what the ancestor forced, so the route layer is not read.
    if upstream.is_some_and(|layer| layer.mode == SharingMode::Enforce) {
        // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-enforce
        let layer = upstream.expect("the upstream layer is present");
        // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-enforce
        return policy_of(&layer.cors);
        // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-enforce-if
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-prevail
    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-enforce-else
    // The ELSE of the enforce check: both layers take part in the overlay.
    // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-enforce-else

    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-inherit
    // Each member is taken from the last layer result that declares it: the
    // origins are already unioned where an ancestor marked the family
    // `inherit`, and the ancestor's value is already withheld where it marked
    // it `private`, so the overlay unions nothing.
    let mut origins: Option<Vec<String>> = None;
    let mut methods: Option<Vec<String>> = None;
    let mut expose: Option<Vec<String>> = None;
    let mut credentials = false;
    let mut enabled = false;
    for layer in [upstream, route].into_iter().flatten() {
        let declared = declared_of(&layer.cors);
        if declared.origins.is_some() {
            origins = declared.origins;
        }
        if declared.methods.is_some() {
            methods = declared.methods;
        }
        if declared.expose.is_some() {
            expose = declared.expose;
        }
        credentials = declared.credentials;
        enabled = layer.cors.enabled;
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-inherit

    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-defaults
    // The shipped schema's declared default for a member neither layer result
    // declares: `GET` and `POST` for the methods, an empty exposure, `false`
    // for the credentials, and no default for the origins, because an absent
    // or empty `allowed_origins` allows no origin rather than every one.
    let origins = origins.unwrap_or_default();
    let methods = methods.unwrap_or_else(default_methods);
    let expose = expose.unwrap_or_default();
    // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-defaults

    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-none-if
    // No layer carries a `cors` object, or the prevailing `enabled` is false:
    // both are the absent-family outcome the flow enforces nothing on.
    if !enabled {
        // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-none
        return None;
        // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-none
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-none-if

    // @cpt-begin:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-return
    Some(EffectiveCorsPolicy {
        enabled: true,
        allowed_origins: origins,
        allowed_methods: methods,
        expose_headers: expose,
        allow_credentials: credentials,
    })
    // @cpt-end:cpt-cf-oagw-algo-cors-fold:p1:inst-cf-return
}

/// The policy one layer's own object states, which the `enforce` branch takes
/// whole.
fn policy_of(cors: &CorsConfig) -> Option<EffectiveCorsPolicy> {
    Some(EffectiveCorsPolicy {
        enabled: cors.enabled,
        allowed_origins: cors.allowed_origins.clone(),
        allowed_methods: if cors.allowed_methods.is_empty() {
            default_methods()
        } else {
            cors.allowed_methods.clone()
        },
        expose_headers: cors.expose_headers.clone(),
        allow_credentials: cors.allow_credentials,
    })
}

/// The problem `detail` of a refusal, which names the offending value and no
/// allowed one.
///
/// The detail strings are the two ADR 0004 gives, and neither names an allowed
/// origin nor an allowed method, so a disallowed caller learns nothing about
/// the list that refused it (§1.5).
#[must_use]
pub fn refusal_detail(reason: CorsRefusal, origin: &str, method: &str) -> String {
    match reason {
        CorsRefusal::Origin => format!("Origin '{origin}' not in allowed origins list"),
        CorsRefusal::Method => format!("Method '{method}' not in allowed methods list"),
    }
}

// @cpt-dod:cpt-cf-oagw-dod-cors-enforcement:p1

/// Decides one actual cross-origin request against the effective policy.
///
/// The origin comparison is the exact matching ADR 0004 states: the whole
/// value against a configured entry, or a `*` entry against anything, with the
/// scheme and the port significant and no pattern, no suffix, no suffix-of,
/// and no case folding performed. The method check runs only after the origin
/// comparison has passed.
#[must_use]
pub fn decide(
    policy: &EffectiveCorsPolicy,
    origin: Option<&str>,
    method: &str,
) -> CorsDecision {
    // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-credwild-if
    // A configuration the write-time validation refused is failed closed here
    // rather than served permissively: the origin set is treated as empty.
    if policy.allow_credentials && policy.allowed_origins.iter().any(|entry| entry == "*") {
        // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-credwild
        return CorsDecision::Refused(CorsRefusal::Origin);
        // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-credwild
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-credwild-if

    // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-origin
    // The whole value must equal an entry, or an entry must be `*`: no
    // pattern, no suffix, no suffix-of, and no case-folding comparison is
    // performed, and no trailing slash is stripped.
    let Some(origin) = origin else {
        return CorsDecision::Refused(CorsRefusal::Origin);
    };
    let allowed = policy
        .allowed_origins
        .iter()
        .any(|entry| entry == "*" || entry == origin);
    // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-origin

    // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-origin-if
    if !allowed {
        // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-origin-refuse
        return CorsDecision::Refused(CorsRefusal::Origin);
        // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-origin-refuse
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-origin-if

    // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-method
    // The method is an exact member test against the literals the shipped
    // schema enumerates, answered only after the origin comparison passed.
    let method_allowed = policy.allowed_methods.iter().any(|entry| entry == method);
    // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-method

    // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-method-if
    if !method_allowed {
        // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-method-refuse
        return CorsDecision::Refused(CorsRefusal::Method);
        // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-method-refuse
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-method-if

    // @cpt-begin:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-allow
    // The decoration echoes the request's own origin, carries the credentials
    // header exactly when the configuration allows them, carries the exposure
    // only when the list names one, and always carries `Vary: Origin`.
    CorsDecision::Allowed(CorsDecoration {
        allow_origin: String::from(origin),
        allow_credentials: policy.allow_credentials,
        expose_headers: policy.expose_headers.clone(),
        vary: VARY_ORIGIN,
    })
    // @cpt-end:cpt-cf-oagw-algo-cors-decide:p1:inst-cd-allow
}

// @cpt-dod:cpt-cf-oagw-dod-cors-preflight:p1

/// Builds the 204 preflight answer from the request's own three header values.
///
/// The routine reads no configuration and resolves no upstream, which is what
/// makes the answer usable when the upstream is unreachable. A value the
/// platform delivered that cannot be formed into a response header arrives as
/// [`None`], and the header that would echo it is omitted from the answer
/// rather than emitted, which is the omission the caller records in the
/// request's execution context (§1.5).
#[must_use]
pub fn preflight_answer(
    origin: Option<&str>,
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> PreflightAnswer {
    let mut headers: Vec<(String, String)> = Vec::new();
    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-origin
    // The request's own origin, byte-exact, omitted when it cannot be formed.
    if let Some(origin) = origin {
        headers.push((
            String::from("Access-Control-Allow-Origin"),
            String::from(origin),
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-origin

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-methods
    // The request's own requested method, byte-exact, omitted when it cannot
    // be formed.
    if let Some(request_method) = request_method {
        headers.push((
            String::from("Access-Control-Allow-Methods"),
            String::from(request_method),
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-methods

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-headers-if
    if let Some(requested) = request_headers {
        // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-headers
        // The requested headers verbatim, with no allowlist applied and no
        // name reordered.
        headers.push((
            String::from("Access-Control-Allow-Headers"),
            String::from(requested),
        ));
        // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-headers
    }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-headers-if

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-headers-else
    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-no-headers
    // The ELSE of the requested-headers check: the header is omitted, because
    // a preflight that names no request header asks about none.
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-no-headers
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-headers-else

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-max-age
    // The constant max age of ADR 0004's preflight example.
    headers.push((
        String::from("Access-Control-Max-Age"),
        String::from(PREFLIGHT_MAX_AGE),
    ));
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-max-age

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-vary
    // The three-member value, so a cache cannot serve one preflight's answer
    // to a request that asked about a different origin, method, or header set.
    headers.push((String::from("Vary"), String::from(PREFLIGHT_VARY)));
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-vary

    // @cpt-begin:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-return
    // The 204 status with that header set and no body.
    PreflightAnswer { status: 204, headers }
    // @cpt-end:cpt-cf-oagw-algo-cors-preflight-headers:p1:inst-cph-return
}
