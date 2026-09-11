//! The pure observability vocabulary of
//! `cpt-cf-oagw-feature-observability`.
//!
//! The module owns the three closed vocabularies the feature emits through and
//! nothing else: the instrument and label-key vocabulary of DESIGN §4.2 that
//! [`crate::infra::observability::MetricInstruments`] registers, the label
//! normalization of `cpt-cf-oagw-algo-label-normalization` that bounds every
//! label value, and the audit-record vocabulary of DESIGN §4.3 that the stdout
//! sink renders. It is pure: no OpenTelemetry type, no sink, no clock beyond the
//! [`std::time`] values a caller hands it and no I/O of any kind, so every
//! decision here is table-testable.
//!
//! The feature emits and never decides: the producing features own the decisions
//! behind every instrument and every record, and this module only fixes the
//! vocabulary they are emitted in and the filter that keeps it bounded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

// @cpt-begin:cpt-cf-oagw-dod-metric-instruments:p1:inst-full
// The instrument vocabulary of `cpt-cf-oagw-dod-metric-instruments`: the nine
// instruments DESIGN §4.2 names that have a producing decision this release,
// their eleven label keys and the twelve histogram buckets. The names are
// literal Prometheus names, the `_total` suffix of a counter and the `s` unit of
// the duration histogram being baked into the constant itself, so no caller can
// compose a name at runtime and no second name for the same series can appear.

/// `oagw_requests_total{host, http.request.method, http.route,
/// http.response.status_code}` — one increment per completed proxied request.
pub const METRIC_REQUESTS_TOTAL: &str = "oagw_requests_total";

/// `oagw_request_duration_seconds{host, http.route, phase}` — one observation
/// per completed pipeline stage, over [`DURATION_BUCKETS`] and no other
/// boundary.
pub const METRIC_REQUEST_DURATION_SECONDS: &str = "oagw_request_duration_seconds";

/// `oagw_requests_in_flight{host}` — incremented when an outbound call is
/// issued and decremented when that request is settled.
pub const METRIC_REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";

/// `oagw_errors_total{host, http.route, error_type}` — one increment per
/// failure the pipeline rendered through a row of the closed 22-row table.
pub const METRIC_ERRORS_TOTAL: &str = "oagw_errors_total";

/// `oagw_rate_limit_exceeded_total{host, path}` — one increment per refusal the
/// rate-limit check produced.
pub const METRIC_RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";

/// `oagw_rate_limit_usage_ratio{host, path}` — the token level a check left
/// behind, within 0.0 to 1.0.
pub const METRIC_RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";

/// `oagw_routing_target_host_used{upstream_id, endpoint_host}` — incremented
/// when a selection consumed an `X-OAGW-Target-Host` value that named the
/// endpoint.
pub const METRIC_ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";

/// `oagw_routing_endpoint_selected{upstream_id, endpoint_host,
/// selection_method}` — one increment per endpoint selection.
pub const METRIC_ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";

/// `oagw_upstream_available{host, endpoint}` — 0 after an exchange the pipeline
/// classified `LinkUnavailable` or `ConnectionTimeout`, 1 after one completed.
pub const METRIC_UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";

/// The three instruments DESIGN §4.2 names that have no producing decision this
/// release. They are named so that the closed vocabulary states them and no
/// registration path can mistake them for an omission: nothing is registered for
/// them, nothing is emitted for them and no placeholder gauge stands in for
/// them (`inst-ob-13`).
pub const UNIMPLEMENTED_INSTRUMENTS: [&str; 3] = [
    "oagw_circuit_breaker_state",
    "oagw_circuit_breaker_transitions_total",
    "oagw_upstream_connections",
];

/// The unit of [`METRIC_REQUEST_DURATION_SECONDS`], baked into the constant.
pub const DURATION_UNIT: &str = "s";

/// The twelve buckets of `oagw_request_duration_seconds`, the only boundaries
/// the histogram defines.
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// `host` — the upstream alias the resolved configuration carries.
pub const LABEL_HOST: &str = "host";

/// `http.request.method` — a standard verb or `_OTHER`.
pub const LABEL_HTTP_REQUEST_METHOD: &str = "http.request.method";

/// `http.route` — the normalized route match pattern, never the raw path.
pub const LABEL_HTTP_ROUTE: &str = "http.route";

/// `http.response.status_code` — the numeric status, never a status class.
pub const LABEL_HTTP_RESPONSE_STATUS_CODE: &str = "http.response.status_code";

/// `phase` — the pipeline stage an observation was taken in.
pub const LABEL_PHASE: &str = "phase";

/// `error_type` — the row name of the closed 22-row table.
pub const LABEL_ERROR_TYPE: &str = "error_type";

/// `path` — the normalized route match pattern of the two rate-limit
/// instruments.
pub const LABEL_PATH: &str = "path";

/// `upstream_id` — the identifier of the resolved configuration.
pub const LABEL_UPSTREAM_ID: &str = "upstream_id";

/// `endpoint_host` — the configured endpoint's host.
pub const LABEL_ENDPOINT_HOST: &str = "endpoint_host";

/// `endpoint` — the configured endpoint's host on the availability gauge.
pub const LABEL_ENDPOINT: &str = "endpoint";

/// `selection_method` — `explicit_header`, `round_robin` or `default`.
pub const LABEL_SELECTION_METHOD: &str = "selection_method";

/// The eleven label keys of §5, the only keys any instrument may carry.
// @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-39
pub const LABEL_KEYS: [&str; 11] = [
    LABEL_HOST,
    LABEL_HTTP_REQUEST_METHOD,
    LABEL_HTTP_ROUTE,
    LABEL_HTTP_RESPONSE_STATUS_CODE,
    LABEL_PHASE,
    LABEL_ERROR_TYPE,
    LABEL_PATH,
    LABEL_UPSTREAM_ID,
    LABEL_ENDPOINT_HOST,
    LABEL_ENDPOINT,
    LABEL_SELECTION_METHOD,
];
// @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-39

/// A pipeline stage of `cpt-cf-oagw-flow-proxy-request`, in that flow's stage
/// order. The `phase` label carries [`Phase::as_str`]; the streamed stages
/// arrive through the same vocabulary, no second one existing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Classification and preflight detection (`inst-pf-01` to `inst-pf-03`).
    Classification,
    /// Config resolution (`inst-pf-04` to `inst-pf-08`).
    ConfigResolution,
    /// Route matching (`inst-pf-09` to `inst-pf-11`).
    RouteMatching,
    /// Endpoint selection (`inst-pf-12` to `inst-pf-15`).
    EndpointSelection,
    /// Actual-request CORS check (`inst-pf-16` to `inst-pf-19`).
    CorsCheck,
    /// Header processing and validation (`inst-pf-20` to `inst-pf-24`).
    HeaderValidation,
    /// Plugin chain with the rate-limit check (`inst-pf-25` to `inst-pf-28`).
    PluginChain,
    /// Scheme and SSRF policy (`inst-pf-29` to `inst-pf-31`).
    SchemePolicy,
    /// Outbound call (`inst-pf-32` to `inst-pf-35`).
    OutboundCall,
    /// Response passthrough (`inst-pf-36` to `inst-pf-38`).
    ResponsePassthrough,
}

impl Phase {
    /// Every stage, in `cpt-cf-oagw-flow-proxy-request`'s stage order.
    pub const ALL: [Phase; 10] = [
        Phase::Classification,
        Phase::ConfigResolution,
        Phase::RouteMatching,
        Phase::EndpointSelection,
        Phase::CorsCheck,
        Phase::HeaderValidation,
        Phase::PluginChain,
        Phase::SchemePolicy,
        Phase::OutboundCall,
        Phase::ResponsePassthrough,
    ];

    /// The `phase` label value of the stage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Classification => "classification",
            Phase::ConfigResolution => "config_resolution",
            Phase::RouteMatching => "route_matching",
            Phase::EndpointSelection => "endpoint_selection",
            Phase::CorsCheck => "actual_request_cors_check",
            Phase::HeaderValidation => "header_processing_and_validation",
            Phase::PluginChain => "plugin_chain_with_rate_limit_check",
            Phase::SchemePolicy => "scheme_and_ssrf_policy",
            Phase::OutboundCall => "outbound_call",
            Phase::ResponsePassthrough => "response_passthrough",
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-metric-instruments:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-cardinality-exposure:p1:inst-full
// The cardinality contract of `cpt-cf-oagw-dod-cardinality-exposure`: the label
// normalization of `cpt-cf-oagw-algo-label-normalization`, applied to every
// update of every instrument so that no label value is unbounded and no tenant,
// subject or principal identifier enters any label set. The label set of an
// instrument is composed here, in the fixed key order of §5, and the
// composition is total: every key of the instrument is resolved for every event
// of that instrument, so no partial label set is ever recorded.

/// One resolved label, a key of [`LABEL_KEYS`] and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// The label key, a `&'static str` of the closed vocabulary.
    pub key: &'static str,
    /// The normalized label value.
    pub value: String,
}

impl Label {
    /// Composes one label from a closed key and a resolved value.
    #[must_use]
    pub const fn new(key: &'static str, value: String) -> Self {
        Self { key, value }
    }
}

/// The label keys no instrument of this feature may carry, the deny-by-default
/// half of the identity rule: an identity is a field of an audit record, never a
/// label of an instrument.
pub const IDENTITY_LABEL_KEYS: [&str; 8] = [
    "tenant_id",
    "tenant",
    "tenant_name",
    "subject_id",
    "subject",
    "principal_id",
    "principal",
    "user_id",
];

/// The label keys that carry a configuration identifier of their own, whose
/// value is the value the persist-time validation left and not an identity.
const IDENTIFIER_LABEL_KEYS: [&str; 3] = [LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST, LABEL_ENDPOINT];

/// True when `value` is the canonical string form of an identifier of this
/// platform: a tenant, a subject and a principal are all UUIDs here, and no
/// other label value of the nine instruments is one.
fn is_identifier_form(value: &str) -> bool {
    // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-38
    value.len() == 36
        && value.as_bytes()[8] == b'-'
        && value.as_bytes()[13] == b'-'
        && value.as_bytes()[18] == b'-'
        && value.as_bytes()[23] == b'-'
        && value
            .bytes()
            .all(|byte| byte == b'-' || byte.is_ascii_hexdigit())
    // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-38
}

/// Takes the event's attributes and the instrument being updated as the inputs
/// (`inst-ob-30`) and the instrument names and label keys of §5 as the only
/// vocabulary: no instrument, no label key and no label value outside them is
/// produced, and no label key is added to any instrument for any event class.
///
/// Every `compose` of this module builds its instrument's whole label set —
/// every key of that instrument resolved for every event of it — and hands it
/// through [`deny_identity`] before the update is recorded, so no partial label
/// set is ever recorded and no label key is ever absent from a recorded series.
///
/// Drops the labels an identity would enter (`inst-ob-38`).
///
/// The filter is deny-by-default and runs on every composed label set: a label
/// whose key is outside the closed vocabulary of §5 is dropped, a label whose
/// key names an identity is dropped, and a label whose value would identify a
/// tenant, a subject or a principal — the canonical identifier form of this
/// platform — is dropped with its label, no substitute being emitted. The
/// identifier a routing instrument legitimately carries, `upstream_id`, is the
/// configuration identifier the persist-time validation left, not an identity,
/// and is the only key whose identifier form survives.
#[must_use]
pub fn deny_identity(labels: Vec<Label>) -> Vec<Label> {
    // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-30
    labels
        .into_iter()
        .filter(|label| {
            LABEL_KEYS.contains(&label.key)
                && !IDENTITY_LABEL_KEYS.contains(&label.key)
                && (IDENTIFIER_LABEL_KEYS.contains(&label.key) || !is_identifier_form(&label.value))
        })
        .collect()
    // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-30
}

/// Normalizes the request method of an instrument label (`inst-ob-33`).
///
/// The proxy shell accepts GET, POST, PUT, PATCH and DELETE; a method outside
/// that set that the router still reports is aggregated as `_OTHER`, the only
/// aggregation the method label allows, so no client can widen the label set by
/// inventing a verb.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-33
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        _ => "_OTHER",
    }
    // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-33
}

/// The route label a request that matched no route carries.
///
/// The value is the registered proxy shell of the gear, the one route the
/// request was answered on, and is a constant: no raw request path, no query
/// string and no client-controlled suffix ever reaches the label.
pub const PROXY_SHELL_ROUTE: &str = "/oagw/v1/proxy/{alias}/{*path}";

/// Normalizes `http.route` of an update (`inst-ob-32`).
///
/// The value is the normalized route match pattern of the route the pipeline
/// matched — its `match.http.path` prefix — so two requests on one route that
/// differ only in their path suffix are counted under one route label and no
/// client can widen the label set by varying its path. A request that matched no
/// route carries [`PROXY_SHELL_ROUTE`], and never the raw request path.
#[must_use]
pub fn normalize_route(route_pattern: Option<&str>) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-32
    route_pattern
        .filter(|pattern| !pattern.is_empty())
        .map_or_else(|| PROXY_SHELL_ROUTE.to_owned(), str::to_owned)
    // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-32
}

/// Normalizes `http.response.status_code` of an update (`inst-ob-34`).
///
/// The value is the numeric status — the upstream status on a passthrough, the
/// gateway status of the mapped row on a gateway-rendered outcome — and no
/// status class is pre-aggregated: a status-class rate is a query-time
/// expression over this label, never a label of its own.
#[must_use]
pub fn normalize_status(status: u16) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-34
    status.to_string()
    // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-34
}

/// Resolves the usage ratio a token level represents over an effective limit
/// (`inst-ob-12`).
///
/// The value is clamped into 0.0 to 1.0, the range
/// `oagw_rate_limit_usage_ratio` is constrained to: an exhausted bucket is 1.0
/// and a limit the resolution did not produce is 0.0.
#[must_use]
pub fn usage_ratio(limit: i64, remaining: i64) -> f64 {
    if limit <= 0 {
        return 0.0;
    }
    let used = limit.saturating_sub(remaining);
    let ratio = used as f64 / limit as f64;
    ratio.clamp(0.0, 1.0)
}

/// The label set of `oagw_requests_total`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOutcomeLabels {
    /// The upstream alias.
    pub host: String,
    /// The normalized method.
    pub method: &'static str,
    /// The normalized route match pattern.
    pub route: String,
    /// The numeric status.
    pub status: u16,
}

impl RequestOutcomeLabels {
    /// Composes the label set in the fixed key order of §5 (`inst-ob-39`).
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-31
        vec![
            Label::new(LABEL_HOST, self.host.clone()),
            Label::new(LABEL_HTTP_REQUEST_METHOD, self.method.to_owned()),
            Label::new(LABEL_HTTP_ROUTE, self.route.clone()),
            Label::new(
                LABEL_HTTP_RESPONSE_STATUS_CODE,
                normalize_status(self.status),
            ),
        ]
        // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-31
    }
}

/// The label set of `oagw_request_duration_seconds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageLabels {
    /// The upstream alias.
    pub host: String,
    /// The normalized route match pattern.
    pub route: String,
    /// The stage the observation was taken in.
    pub phase: Phase,
}

impl StageLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        vec![
            Label::new(LABEL_HOST, self.host.clone()),
            Label::new(LABEL_HTTP_ROUTE, self.route.clone()),
            Label::new(LABEL_PHASE, self.phase.as_str().to_owned()),
        ]
    }
}

/// The label set of `oagw_requests_in_flight`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightLabels {
    /// The upstream alias.
    pub host: String,
}

impl InFlightLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        vec![Label::new(LABEL_HOST, self.host.clone())]
    }
}

/// The label set of `oagw_errors_total`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLabels {
    /// The upstream alias.
    pub host: String,
    /// The normalized route match pattern.
    pub route: String,
    /// The row name of the closed 22-row table (`inst-ob-35`).
    pub error_type: &'static str,
}

impl ErrorLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-35
        vec![
            Label::new(LABEL_HOST, self.host.clone()),
            Label::new(LABEL_HTTP_ROUTE, self.route.clone()),
            Label::new(LABEL_ERROR_TYPE, self.error_type.to_owned()),
        ]
        // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-35
    }
}

/// The label set of `oagw_rate_limit_exceeded_total` and of
/// `oagw_rate_limit_usage_ratio`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitLabels {
    /// The upstream alias.
    pub host: String,
    /// The normalized route match pattern (`inst-ob-37`).
    pub path: String,
}

impl RateLimitLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-37
        vec![
            Label::new(LABEL_HOST, self.host.clone()),
            Label::new(LABEL_PATH, self.path.clone()),
        ]
        // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-37
    }
}

/// The label set of `oagw_routing_target_host_used`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetHostLabels {
    /// The identifier of the resolved configuration (`inst-ob-36`).
    pub upstream_id: String,
    /// The configured endpoint's host.
    pub endpoint_host: String,
}

impl TargetHostLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        vec![
            Label::new(LABEL_UPSTREAM_ID, self.upstream_id.clone()),
            Label::new(LABEL_ENDPOINT_HOST, self.endpoint_host.clone()),
        ]
    }
}

/// The label set of `oagw_routing_endpoint_selected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionLabels {
    /// The identifier of the resolved configuration.
    pub upstream_id: String,
    /// The configured endpoint's host.
    pub endpoint_host: String,
    /// The method the selection used.
    pub selection_method: &'static str,
}

impl SelectionLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        // @cpt-begin:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-36
        vec![
            Label::new(LABEL_UPSTREAM_ID, self.upstream_id.clone()),
            Label::new(LABEL_ENDPOINT_HOST, self.endpoint_host.clone()),
            Label::new(LABEL_SELECTION_METHOD, self.selection_method.to_owned()),
        ]
        // @cpt-end:cpt-cf-oagw-algo-label-normalization:p1:inst-ob-36
    }
}

/// The label set of `oagw_upstream_available`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityLabels {
    /// The upstream alias.
    pub host: String,
    /// The configured endpoint's host.
    pub endpoint: String,
}

impl AvailabilityLabels {
    /// Composes the label set in the fixed key order of §5.
    #[must_use]
    pub fn compose(&self) -> Vec<Label> {
        vec![
            Label::new(LABEL_HOST, self.host.clone()),
            Label::new(LABEL_ENDPOINT, self.endpoint.clone()),
        ]
    }
}
// @cpt-end:cpt-cf-oagw-dod-cardinality-exposure:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-audit-record:p1:inst-full
// The audit-record contract of `cpt-cf-oagw-dod-audit-record`: the closed
// 14-field base set of DESIGN §4.3 plus the failed-request `error_message`, the
// redaction of `cpt-cf-oagw-algo-audit-redaction` and the record state machine
// of `cpt-cf-oagw-state-audit-record`. The field set is closed, so a record a
// redaction rule touches is suppressed rather than written short a field and no
// partial record is ever emitted.

/// The level of an audit record, the vocabulary DESIGN §4.3 fixes (`inst-ob-46`).
///
/// DEBUG is absent from the vocabulary on purpose: the level DESIGN defines for
/// detailed plugin execution is disabled in production and produces no record in
/// this release, so no DEBUG record can ever be produced here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditLevel {
    /// Successful requests and normal operations.
    Info,
    /// A refusal that carries retry guidance.
    Warn,
    /// An upstream failure, a timeout or an authentication failure.
    Error,
}

impl AuditLevel {
    /// The JSON value of the level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AuditLevel::Info => "INFO",
            AuditLevel::Warn => "WARN",
            AuditLevel::Error => "ERROR",
        }
    }
}

/// The Description column of the closed 22-row table, keyed by row name.
///
/// `error_message` of a failed request is the mapped row's `title` and `detail`
/// of the closed table (`inst-ob-44`) — its own fixed text, never the
/// occurrence content that triggered the row (`inst-ob-42`), so a `cred://`
/// reference an occurrence names stays out of the record and two occurrences of
/// the same failure render byte-identical `error_message` values.
const ROW_DESCRIPTIONS: &[(&str, &str)] = &[
    ("RouteError", "General route validation error"),
    ("ValidationError", "Request validation failed"),
    (
        "MissingTargetHost",
        "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias",
    ),
    (
        "InvalidTargetHost",
        "X-OAGW-Target-Host header format is invalid (must be hostname or IP, no port/path/special chars)",
    ),
    (
        "UnknownTargetHost",
        "X-OAGW-Target-Host value does not match any configured endpoint",
    ),
    ("AuthenticationFailed", "Authentication to upstream failed"),
    ("RouteNotFound", "No matching route found"),
    ("PluginInUse", "Plugin in use"),
    ("PayloadTooLarge", "Request payload exceeds limit"),
    ("RateLimitExceeded", "Rate limit exceeded"),
    ("SecretNotFound", "Referenced secret not found"),
    ("ProtocolError", "Protocol-level error"),
    ("DownstreamError", "Upstream service error"),
    ("StreamAborted", "Stream connection aborted"),
    ("LinkUnavailable", "Upstream link unavailable"),
    ("CircuitBreakerOpen", "Circuit breaker open"),
    ("PluginNotFound", "Plugin not found"),
    ("ConnectionTimeout", "Connection timed out"),
    ("RequestTimeout", "Request timed out"),
    ("IdleTimeout", "Idle timeout"),
    (
        "CorsOriginNotAllowed",
        "Origin not allowed by the route CORS policy",
    ),
    (
        "CorsMethodNotAllowed",
        "Method not allowed by the route CORS policy",
    ),
];

/// The Description column of the row the closed table names.
fn row_description(error_type: &str) -> Option<&'static str> {
    ROW_DESCRIPTIONS
        .iter()
        .find(|(name, _)| *name == error_type)
        .map(|(_, description)| *description)
}

/// One row of the closed 22-row table a failure was rendered through.
///
/// `error_type` is the row name and `error_message` the row's `title` and
/// `detail` of the closed table (`inst-ob-44`): the row's own fixed text, never
/// occurrence content, so two occurrences of the same failure produce
/// byte-identical values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditErrorRow {
    /// The row name of the closed table.
    pub error_type: &'static str,
    /// The row's `title` and `detail` of the closed table, copied verbatim.
    pub error_message: String,
}

impl AuditErrorRow {
    /// Builds the row a rendered failure carries.
    #[must_use]
    pub fn of(error_type: &'static str, title: &str) -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-44
        let error_message = match row_description(error_type) {
            Some(description) => format!("{title}: {description}"),
            None => title.to_owned(),
        };
        Self {
            error_type,
            error_message,
        }
        // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-44
    }
}

/// The row of the closed 22-row table a rate-limit refusal is rendered through
/// (`inst-ob-21`), the row the refusal class names.
const RATE_LIMIT_REFUSAL_ROW: &str = "RateLimitExceeded";

/// The row of the closed table the refusal class is rendered through, with the
/// `Retry-After` delay the check computed carried in the row's own fixed text
/// (`inst-ob-21`).
///
/// The field set of the record is closed (`inst-ob-40`) and no field is added
/// for a rate-limit refusal, so the delay has no member of its own: it is
/// appended to the row's fixed text, which is the only place in the record a
/// value of this class can name it. The `title` the row is rendered from is
/// read from the authoritative table rather than re-declared here.
fn refusal_error_row(retry_after_secs: u64) -> AuditErrorRow {
    let title = crate::domain::error::MAPPING_TABLE
        .iter()
        .find(|row| row.variant == RATE_LIMIT_REFUSAL_ROW)
        .map_or(RATE_LIMIT_REFUSAL_ROW, |row| row.title);
    let mut row = AuditErrorRow::of(RATE_LIMIT_REFUSAL_ROW, title);
    row.error_message = format!("{} retry_after_secs={retry_after_secs}", row.error_message);
    row
}

/// The event class of one audit record, the vocabulary of DESIGN §4.3's "What is
/// Logged" block that has a producer this release.
///
/// The circuit-breaker class DESIGN names has no producer this release and is
/// absent from the vocabulary, so no circuit-breaker event can be opened
/// (`inst-ob-13`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEventClass {
    /// A create, replace or delete of an upstream, route or plugin resource of
    /// the management API, an enable or a disable included — they are replaces.
    /// The endpoint, plugin-binding and CORS material such a decision carries is
    /// nested fields of the resource, not surfaces of their own.
    ManagementChange {
        /// The operation class: `create`, `replace` or `delete`.
        operation: &'static str,
        /// The resource the decision is on: `upstream`, `route` or `plugin`.
        resource: &'static str,
    },
    /// A credential-resolution failure the auth phase mapped onto one of its
    /// rows, or the upstream's rejection of the credentials it injected.
    AuthenticationFailure,
    /// The refusal the rate-limit check produced on a proxied request.
    RateLimitRefusal {
        /// The `Retry-After` delay the check computed.
        retry_after_secs: u64,
    },
    /// A failed proxied request: a gateway-rendered row, or a passed-through
    /// upstream error status the pipeline handed back without rendering a row
    /// for it.
    RequestFailure {
        /// The passed-through numeric status, when the failure is
        /// upstream-originated.
        passed_through_status: Option<u16>,
    },
    /// A successful proxied request, the only class the sampling gate may drop.
    RequestSuccess,
}

impl AuditEventClass {
    /// The `event` value the class opens a record with (`inst-ob-19`).
    #[must_use]
    pub fn event_name(&self) -> String {
        match self {
            AuditEventClass::ManagementChange {
                operation,
                resource,
            } => format!("{operation}_{resource}"),
            AuditEventClass::AuthenticationFailure => "auth_failure".to_owned(),
            AuditEventClass::RateLimitRefusal { .. } => "rate_limit_refusal".to_owned(),
            AuditEventClass::RequestFailure { .. } | AuditEventClass::RequestSuccess => {
                "proxy_request".to_owned()
            }
        }
    }

    /// The level the class is recorded at (`inst-ob-46`).
    #[must_use]
    pub const fn level(&self) -> AuditLevel {
        // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-46
        match self {
            AuditEventClass::ManagementChange { .. } => AuditLevel::Info,
            AuditEventClass::AuthenticationFailure => AuditLevel::Error,
            AuditEventClass::RateLimitRefusal { .. } => AuditLevel::Warn,
            AuditEventClass::RequestFailure { .. } => AuditLevel::Error,
            AuditEventClass::RequestSuccess => AuditLevel::Info,
        }
        // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-46
    }

    /// True when the class is the only class the sampling gate may drop.
    #[must_use]
    pub const fn is_sampled(&self) -> bool {
        matches!(self, AuditEventClass::RequestSuccess)
    }

    /// True when the class is the class the authentication rate limit applies
    /// to, so a flood of failed authentications cannot flood the log.
    #[must_use]
    pub const fn is_rate_limited(&self) -> bool {
        matches!(self, AuditEventClass::AuthenticationFailure)
    }

    /// True when the class is a failure a row of the closed table was rendered
    /// for, the only records that carry `error_message`.
    #[must_use]
    pub const fn is_failed_request(&self) -> bool {
        matches!(self, AuditEventClass::RequestFailure { .. })
    }

    /// True when the class is a failure the closed table named, the only
    /// records that carry `error_type` and `error_message`.
    #[must_use]
    pub const fn carries_error(&self) -> bool {
        matches!(
            self,
            AuditEventClass::RequestFailure { .. }
                | AuditEventClass::RateLimitRefusal { .. }
                | AuditEventClass::AuthenticationFailure
        )
    }
}

/// The one audit record the feature writes, carrying the 14 base fields of
/// DESIGN §4.3 plus `error_message` on a failed request and no field outside
/// that set (`inst-ob-40`).
// @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-40
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// RFC 3339 instant the record was produced at, from a `SystemTime`.
    pub timestamp: String,
    /// The level of the record.
    pub level: AuditLevel,
    /// The event name the class fixed.
    pub event: String,
    /// The platform trace context the request arrived with (`inst-ob-45`), the
    /// only header-shaped value in any record.
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-45
    pub request_id: Option<String>,
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-45
    /// The tenant the record is about.
    pub tenant_id: Option<String>,
    /// The authenticated subject of the security context.
    pub principal_id: Option<String>,
    /// The upstream alias the resolved configuration carries.
    pub host: String,
    /// The gear-relative request path, with no query string (`inst-ob-43`).
    pub path: String,
    /// The normalized request method.
    pub method: String,
    /// The numeric status.
    pub status: u16,
    /// The whole-request duration, measured from the arrival instant.
    pub duration_ms: u64,
    /// The size of the received request body.
    pub request_size: u64,
    /// The size of the body handed back.
    pub response_size: u64,
    /// The row name of the closed table, unset when no row was rendered.
    pub error_type: Option<String>,
    /// The row's `title` and `detail`, present only on a failed request.
    pub error_message: Option<String>,
}

impl AuditEvent {
    /// Serializes the record as exactly one structured JSON line.
    ///
    /// The members are emitted in the field order the ADR's "Audit Log JSON
    /// Format" block fixes; the 14 base keys are always present, `error_type`
    /// being `null` when no row was rendered, and `error_message` is present
    /// only on a failed request.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let mut line = String::with_capacity(256);
        line.push('{');
        // The members are separated by a comma only between two of them, so the
        // line is one well-formed JSON object and never ends in a comma.
        let mut first = true;
        let mut member = |line: &mut String, rendered: String| {
            if !first {
                line.push(',');
            }
            first = false;
            line.push_str(&rendered);
        };
        member(&mut line, string_member("timestamp", &self.timestamp));
        member(&mut line, string_member("level", self.level.as_str()));
        member(&mut line, string_member("event", &self.event));
        member(
            &mut line,
            optional_member("request_id", self.request_id.as_deref()),
        );
        member(
            &mut line,
            optional_member("tenant_id", self.tenant_id.as_deref()),
        );
        member(
            &mut line,
            optional_member("principal_id", self.principal_id.as_deref()),
        );
        member(&mut line, string_member("host", &self.host));
        member(&mut line, string_member("path", &self.path));
        member(&mut line, string_member("method", &self.method));
        member(&mut line, number_member("status", u64::from(self.status)));
        member(&mut line, number_member("duration_ms", self.duration_ms));
        member(&mut line, number_member("request_size", self.request_size));
        member(
            &mut line,
            number_member("response_size", self.response_size),
        );
        member(
            &mut line,
            optional_member("error_type", self.error_type.as_deref()),
        );
        if let Some(message) = self.error_message.as_deref() {
            member(&mut line, string_member("error_message", message));
        }
        line.push('}');
        line
    }
}
// @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-40

/// Renders one escaped JSON string member.
fn string_member(key: &str, value: &str) -> String {
    format!("\"{key}\":\"{}\"", json_escape(value))
}

/// Renders one JSON string member that is `null` when the value is absent.
fn optional_member(key: &str, value: Option<&str>) -> String {
    value.map_or_else(
        || format!("\"{key}\":null"),
        |value| string_member(key, value),
    )
}

/// Renders one JSON number member.
fn number_member(key: &str, value: u64) -> String {
    format!("\"{key}\":{value}")
}

/// Escapes a value for a JSON string member.
#[must_use]
pub fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

/// Formats a `SystemTime` as the RFC 3339 instant a `timestamp` field carries.
#[must_use]
pub fn rfc3339(at: SystemTime) -> String {
    let elapsed = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((elapsed.as_secs() / 86_400) as i64);
    let seconds_of_day = elapsed.as_secs() % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
        elapsed.subsec_millis()
    )
}

/// Converts a day count since the Unix epoch into a civil date, the proleptic
/// Gregorian algorithm the RFC 3339 form is rendered from.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_pointer = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_pointer + 2) / 153 + 1) as u32;
    let month = if month_pointer < 10 {
        month_pointer + 3
    } else {
        month_pointer - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The fill material a producing path hands to the audit flow (`inst-ob-17`):
/// the values the path already holds, nothing re-read from a store and nothing
/// re-derived here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFacts {
    /// The platform trace context the request arrived with.
    pub request_id: Option<String>,
    /// The tenant the record is about.
    pub tenant_id: Option<String>,
    /// The authenticated subject of the security context.
    pub principal_id: Option<String>,
    /// The upstream alias the resolved configuration carries.
    pub host: String,
    /// The gear-relative request path.
    pub path: String,
    /// The normalized request method.
    pub method: String,
    /// The numeric status.
    pub status: u16,
    /// The whole-request duration, measured from the arrival instant.
    pub duration_ms: u64,
    /// The size of the received request body.
    pub request_size: u64,
    /// The size of the body handed back.
    pub response_size: u64,
    /// The row a failure was rendered through, when one was.
    pub error: Option<AuditErrorRow>,
}

/// The sampling counters the feature holds in process-local memory
/// (`inst-ob-47`, `inst-ob-48`), a restart resetting both.
///
/// The counters are shared by every emission path of a process, so the
/// successful-request counter is atomic and the authentication-failure windows
/// carry the only lock among them: a class the gate cannot drop — a failure, a
/// refusal, a configuration change — is emitted unsampled and reaches neither.
#[derive(Debug, Default)]
pub struct SamplingCounters {
    /// The successful-request records the sampling gate has counted.
    successes: AtomicU64,
    /// The instants the authentication-failure records were admitted at, one
    /// window per tenant the record is about (`inst-ob-48`), so a flood from
    /// one source cannot spend another tenant's budget.
    authentications: Mutex<HashMap<String, Vec<Instant>>>,
}

impl SamplingCounters {
    /// The empty counters a process starts with.
    #[must_use]
    pub fn new() -> Self {
        Self {
            successes: AtomicU64::new(0),
            authentications: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one successful-request record and reports whether the 1/100
    /// posture admits it (`inst-ob-47`): one record in N is emitted and the
    /// rest are dropped.
    ///
    /// inst-ob-50: a dropped record is dropped without compensation — no counter
    /// is incremented for it, nothing is queued, batched or retried, and the
    /// instruments of §5 are unaffected by the sampling, because the counters
    /// record every request and only the successful-request records are
    /// sampled.
    pub fn admit_success(&self) -> bool {
        // The counter is the only thing a success reaches, so it is a bare
        // increment: no lock is taken on the path the gate drops ~99% of.
        let counted = self
            .successes
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        counted.is_multiple_of(u64::from(SUCCESS_SAMPLE))
    }

    /// Applies the rate limit the authentication-failure class is recorded
    /// under (`inst-ob-48`): a sliding one-second window over the injected
    /// [`Instant`], admitting at most [`AUTH_AUDIT_PER_SEC`] records in any one
    /// window, so a flood of failed authentications cannot flood the log.
    ///
    /// The window the record is admitted under is the tenant the record is
    /// about, the principal standing in when the record carries no tenant, so
    /// one source exhausting its own budget leaves every other tenant's budget
    /// whole. The windows are bounded: a key beyond `AUTH_AUDIT_WINDOWS` shares
    /// the unkeyed window rather than growing the counters without limit, the
    /// field set of the record being the only vocabulary a key can be taken
    /// from.
    pub fn admit_authentication(&self, key: &str, now: Instant) -> bool {
        let mut windows = self.authentications.lock();
        // A window a second old is emptied and dropped with it, so the counters
        // hold no more windows than the current second produced.
        windows.retain(|_, admitted| {
            admitted.retain(|instant| now.duration_since(*instant) < Duration::from_secs(1));
            !admitted.is_empty()
        });
        let key = if windows.contains_key(key) || windows.len() < AUTH_AUDIT_WINDOWS {
            key
        } else {
            UNKEYED_WINDOW
        };
        let window = windows.entry(key.to_owned()).or_default();
        if window.len() >= AUTH_AUDIT_PER_SEC as usize {
            return false;
        }
        window.push(now);
        true
    }
}

/// The window an authentication-failure record carrying no tenant and no
/// principal is admitted under, the one window whose budget is process-global.
const UNKEYED_WINDOW: &str = "";

/// The bound on the authentication-failure windows the counters hold, a
/// feature-local constant of the implementation and not a key of `OagwConfig`:
/// a record whose key falls beyond it shares the unkeyed window, so no input
/// can grow the counters without limit.
const AUTH_AUDIT_WINDOWS: usize = 256;

/// The window an authentication-failure record is admitted under: the tenant
/// the record is about, the principal standing in when the record carries no
/// tenant, and the unkeyed window when it carries neither (`inst-ob-48`).
fn authentication_key(event: &AuditEvent) -> String {
    // The default is the unkeyed window, the one window whose budget is
    // process-global.
    event
        .tenant_id
        .clone()
        .or_else(|| event.principal_id.clone())
        .unwrap_or_default()
}

/// The feature-local 1/100 sampling posture DESIGN §4.3 names, and not a key of
/// `OagwConfig`.
pub const SUCCESS_SAMPLE: u32 = 100;

/// The authentication-failure rate limit DESIGN §4.3 applies to that class, in
/// records per second, and not a key of `OagwConfig`.
pub const AUTH_AUDIT_PER_SEC: u32 = 5;

/// The outcome the redaction and rendering filter takes back to the audit flow.
// @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-50
// A dropped record is dropped without compensation: no counter is incremented
// for it, nothing is queued, batched or retried, and the instruments of §5 are
// unaffected by the sampling, because the counters record every request and only
// the successful-request records are sampled. The suppressed outcome is the only
// thing a dropped record leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    /// A record ready to write, boxed so the outcome stays small enough to
    /// cross a `Result` boundary.
    Rendered(Box<AuditEvent>),
    /// The record was dropped whole: no substitute value was emitted, nothing
    /// was buffered for a later write and no counter was incremented for it.
    Suppressed,
}
// @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-50

/// The state of one audit record in `cpt-cf-oagw-state-audit-record`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditRecordState {
    /// The record's fields are being filled from the values the producing path
    /// holds.
    Collected,
    /// The redaction filter returned the record ready to write.
    Rendered,
    /// The record was dropped whole.
    Suppressed,
    /// The single structured JSON line reached stdout.
    Written,
}

impl AuditRecordState {
    /// The four declared transitions of the state machine, and no other
    /// (`inst-ob-52` to `inst-ob-55`).
    #[must_use]
    pub const fn transition(self, to: Self) -> Option<Self> {
        match (self, to) {
            // @cpt-begin:cpt-cf-oagw-state-audit-record:p1:inst-ob-52
            // Collected -> Rendered: the fields are filled and the filter
            // returned the record ready to write.
            (Self::Collected, Self::Rendered) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-audit-record:p1:inst-ob-52
            // @cpt-begin:cpt-cf-oagw-state-audit-record:p1:inst-ob-53
            // Collected -> Suppressed: a field value would carry a body, a
            // query string, a header value or credential material, the record
            // being dropped before it is rendered.
            (Self::Collected, Self::Suppressed) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-audit-record:p1:inst-ob-53
            // @cpt-begin:cpt-cf-oagw-state-audit-record:p1:inst-ob-54
            // Rendered -> Written: the JSON line reached stdout.
            (Self::Rendered, Self::Written) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-audit-record:p1:inst-ob-54
            // @cpt-begin:cpt-cf-oagw-state-audit-record:p1:inst-ob-55
            // Rendered -> Suppressed: the sampling gate dropped a successful
            // request's record, or the write to stdout failed.
            (Self::Rendered, Self::Suppressed) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-audit-record:p1:inst-ob-55
            _ => None,
        }
    }
}

/// Renders one filled record under the redaction and sampling filter
/// (`inst-ob-40` to `inst-ob-51`), taking back either a record ready to write
/// or the suppressed outcome.
///
/// The field set is closed, so a record the forbidden-value rule touches is
/// suppressed whole rather than written short a field; the sampling gate runs
/// inside this filter, so a sampled-out success is suppressed before a line
/// exists; and every other class is written every time it happens. The counters
/// are taken by shared reference: the successful-request counter and the
/// authentication-failure windows carry their own synchronization, so a class
/// the gate cannot drop — a failure, a refusal, a configuration change — is
/// emitted unsampled without any of them being reached.
pub fn render(
    event: AuditEvent,
    class: &AuditEventClass,
    counters: &SamplingCounters,
    now: Instant,
) -> AuditOutcome {
    let mut event = event;
    // inst-ob-21: the refusal class opens the WARN record DESIGN §4.3 assigns to
    // that log point, with `error_type` `RateLimitExceeded` and the
    // `Retry-After` delay the check computed. The field set is closed, so the
    // delay is carried in the row's own fixed text and nowhere else.
    if let AuditEventClass::RateLimitRefusal { retry_after_secs } = class {
        let row = refusal_error_row(*retry_after_secs);
        event.error_type = Some(row.error_type.to_owned());
        event.error_message = Some(row.error_message);
    }
    // A field value that would carry a request body, a response body, a query
    // string, a query parameter, a header value or credential material drops the
    // record whole: no PII and no secrets, and no substitute value is emitted
    // (inst-ob-41, inst-ob-42).
    let state = AuditRecordState::Collected;
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-42
    if forbidden_marker(&event).is_some() {
        // inst-ob-53: a field value would carry a body, a query string, a
        // header value or credential material and no substitute value exists,
        // so the record is dropped before it is rendered — the `Collected` to
        // `Suppressed` transition, which the sampling gate takes below too.
        let _ = state.transition(AuditRecordState::Suppressed);
        return AuditOutcome::Suppressed;
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-42
    // The level the record carries is the one the class fixed, and never DEBUG:
    // the vocabulary of `AuditLevel` has no DEBUG variant, so no caller can
    // lower a record into it (inst-ob-46).
    let Some(state) = state.transition(AuditRecordState::Rendered) else {
        return AuditOutcome::Suppressed;
    };
    // The sampling posture: only the successful-request class is admitted to the
    // gate, one record in N being emitted and the rest dropped (inst-ob-47).
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-47
    if class.is_sampled() && !counters.admit_success() {
        return AuditOutcome::Suppressed;
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-47
    // The authentication-failure class is rate-limited before it is written, so
    // a flood of failed authentications cannot flood the log (inst-ob-48). The
    // window the record is admitted under is the tenant's own, so one flood
    // source cannot spend another tenant's budget.
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-48
    if class.is_rate_limited() && !counters.admit_authentication(&authentication_key(&event), now) {
        let _ = state.transition(AuditRecordState::Suppressed);
        return AuditOutcome::Suppressed;
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-48
    // Every other class is emitted unsampled: every failed request, every
    // refused request, every timeout and every management configuration change
    // is written every time it happens (inst-ob-49).
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-49
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-51
    AuditOutcome::Rendered(Box::new(event))
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-51
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-49
}

/// Names the marker whose presence in a field value means the value carries a
/// request body, a response body, a query string, a query parameter, a header
/// value or credential material.
///
/// The field set is closed and every field is filled from a value the producing
/// path holds, so the rule is a deny-by-default scan over the values that are
/// present: a value that carries one of these forms means the record cannot be
/// written, and it is suppressed whole.
const FORBIDDEN_MARKERS: [&str; 8] = [
    "cred://",
    "bearer ",
    "basic ",
    "authorization:",
    "cookie:",
    "api_key=",
    "?",
    "\n",
];

/// Returns the marker a field value of the record carries, if any.
fn forbidden_marker(event: &AuditEvent) -> Option<&'static str> {
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-41
    let values = [
        event.event.as_str(),
        event.request_id.as_deref().unwrap_or_default(),
        event.tenant_id.as_deref().unwrap_or_default(),
        event.principal_id.as_deref().unwrap_or_default(),
        event.host.as_str(),
        event.path.as_str(),
        event.method.as_str(),
        event.error_type.as_deref().unwrap_or_default(),
        event.error_message.as_deref().unwrap_or_default(),
    ];
    FORBIDDEN_MARKERS.iter().find_map(|marker| {
        values
            .iter()
            .any(|value| value.contains(marker))
            .then_some(*marker)
    })
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-41
}

/// Resolves the `path` field of a proxied request (`inst-ob-43`).
///
/// The value is the request path the request arrived on, in the gear-relative
/// `/oagw/v1/...` form, with no query string appended and never in the
/// `/api`-prefixed alias form.
#[must_use]
pub fn audit_path(alias: &str, path_suffix: &str) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-43
    let suffix = path_suffix.split(['?', '#']).next().unwrap_or_default();
    let suffix = suffix.strip_suffix('/').unwrap_or(suffix);
    if suffix.is_empty() || suffix == "/" {
        format!("/oagw/v1/proxy/{alias}")
    } else {
        format!("/oagw/v1/proxy/{alias}{suffix}")
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-redaction:p1:inst-ob-43
}

/// Resolves the `path` field of a management surface (`inst-ob-43`).
///
/// The value is the gear-relative path the management API registers, with no
/// query string and never the `/api`-prefixed alias form.
#[must_use]
pub fn audit_management_path(path: &str) -> String {
    path.split(['?', '#']).next().unwrap_or_default().to_owned()
}
// @cpt-end:cpt-cf-oagw-dod-audit-record:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::OagwError;

    // ------------------------------------------------------------------
    // The instrument and label-key vocabulary
    // ------------------------------------------------------------------

    #[test]
    fn the_nine_names_are_the_prometheus_names_of_the_vocabulary() {
        assert_eq!(METRIC_REQUESTS_TOTAL, "oagw_requests_total");
        assert_eq!(
            METRIC_REQUEST_DURATION_SECONDS,
            "oagw_request_duration_seconds"
        );
        assert_eq!(METRIC_REQUESTS_IN_FLIGHT, "oagw_requests_in_flight");
        assert_eq!(METRIC_ERRORS_TOTAL, "oagw_errors_total");
        assert_eq!(
            METRIC_RATE_LIMIT_EXCEEDED_TOTAL,
            "oagw_rate_limit_exceeded_total"
        );
        assert_eq!(METRIC_RATE_LIMIT_USAGE_RATIO, "oagw_rate_limit_usage_ratio");
        assert_eq!(
            METRIC_ROUTING_TARGET_HOST_USED,
            "oagw_routing_target_host_used"
        );
        assert_eq!(
            METRIC_ROUTING_ENDPOINT_SELECTED,
            "oagw_routing_endpoint_selected"
        );
        assert_eq!(METRIC_UPSTREAM_AVAILABLE, "oagw_upstream_available");
        // A counter bakes its `_total` suffix into the constant itself.
        assert!(METRIC_REQUESTS_TOTAL.ends_with("_total"));
        assert!(METRIC_ERRORS_TOTAL.ends_with("_total"));
        assert!(METRIC_RATE_LIMIT_EXCEEDED_TOTAL.ends_with("_total"));
        // The duration histogram bakes its unit into the constant.
        assert_eq!(
            METRIC_REQUEST_DURATION_SECONDS,
            "oagw_request_duration_seconds"
        );
    }

    #[test]
    fn the_label_keys_are_the_eleven_keys_of_the_vocabulary() {
        let keys = [
            LABEL_HOST,
            LABEL_HTTP_REQUEST_METHOD,
            LABEL_HTTP_ROUTE,
            LABEL_HTTP_RESPONSE_STATUS_CODE,
            LABEL_PHASE,
            LABEL_ERROR_TYPE,
            LABEL_PATH,
            LABEL_UPSTREAM_ID,
            LABEL_ENDPOINT_HOST,
            LABEL_ENDPOINT,
            LABEL_SELECTION_METHOD,
        ];
        assert_eq!(LABEL_KEYS, keys);
        // No tenant, subject or principal key exists in the vocabulary at all.
        for key in LABEL_KEYS {
            assert!(
                !key.to_ascii_lowercase().contains("tenant"),
                "no tenant label key: {key}"
            );
            assert!(
                !key.to_ascii_lowercase().contains("subject"),
                "no subject label key: {key}"
            );
            assert!(
                !key.to_ascii_lowercase().contains("principal"),
                "no principal label key: {key}"
            );
        }
    }

    #[test]
    fn the_histogram_has_exactly_the_twelve_buckets() {
        assert_eq!(DURATION_BUCKETS.len(), 12);
        assert_eq!(
            DURATION_BUCKETS,
            [
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0
            ]
        );
        assert_eq!(DURATION_UNIT, "s");
    }

    #[test]
    fn the_three_unimplemented_instruments_are_named_and_none_other() {
        assert_eq!(
            UNIMPLEMENTED_INSTRUMENTS,
            [
                "oagw_circuit_breaker_state",
                "oagw_circuit_breaker_transitions_total",
                "oagw_upstream_connections"
            ]
        );
    }

    // ------------------------------------------------------------------
    // The label normalization
    // ------------------------------------------------------------------

    #[test]
    fn the_host_label_is_the_upstream_alias() {
        // `inst-ob-31`: the alias the resolved configuration carries, never the
        // caller's authority, the endpoint host or the Host header.
        let labels = RequestOutcomeLabels {
            host: "payments".to_owned(),
            method: "GET",
            route: normalize_route(Some("/v1/payments")),
            status: 200,
        }
        .compose();
        assert_eq!(labels[0].key, LABEL_HOST);
        assert_eq!(labels[0].value, "payments");
    }

    #[test]
    fn the_route_label_is_the_match_pattern_never_the_raw_path() {
        assert_eq!(
            normalize_route(Some("/v1/payments/{id}")),
            "/v1/payments/{id}"
        );
        // A request that matched no route carries the registered shell, never
        // the raw path it arrived on.
        assert_eq!(normalize_route(None), PROXY_SHELL_ROUTE);
        assert_eq!(normalize_route(Some("")), PROXY_SHELL_ROUTE);
        // A pattern the domain model persisted is carried verbatim; the raw
        // request path never reaches this function as a fallback value.
        assert_eq!(
            normalize_route(Some("/api/v1/payments/{id}")),
            "/api/v1/payments/{id}"
        );
    }

    #[test]
    fn a_method_outside_the_standard_set_is_aggregated_as_other() {
        for verb in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            assert_eq!(normalize_method(verb), verb);
        }
        for verb in ["HEAD", "OPTIONS", "TRACE", "CONNECT", "PROPFIND", ""] {
            assert_eq!(normalize_method(verb), "_OTHER");
        }
    }

    #[test]
    fn the_status_label_is_numeric_and_never_a_status_class() {
        assert_eq!(normalize_status(200), "200");
        assert_eq!(normalize_status(404), "404");
        assert_eq!(normalize_status(503), "503");
        for class in ["2xx", "4xx", "5xx"] {
            assert_ne!(
                normalize_status(200),
                class,
                "no status class is pre-aggregated"
            );
        }
    }

    #[test]
    fn the_phase_label_carries_the_stage_names_in_the_flow_order() {
        let names: Vec<&str> = Phase::ALL.iter().map(|phase| phase.as_str()).collect();
        assert_eq!(
            names,
            [
                "classification",
                "config_resolution",
                "route_matching",
                "endpoint_selection",
                "actual_request_cors_check",
                "header_processing_and_validation",
                "plugin_chain_with_rate_limit_check",
                "scheme_and_ssrf_policy",
                "outbound_call",
                "response_passthrough",
            ]
        );
    }

    #[test]
    fn no_label_of_any_instrument_carries_a_tenant_or_a_principal() {
        // A tenant identifier, a subject identifier and a principal identifier
        // are dropped whole from every label set (inst-ob-38): the label set of
        // every instrument is composed from the alias, the route, the method,
        // the status, the stage, the row name and the endpoint identifiers
        // alone, and nothing else a producer holds reaches a label.
        let tenant = "0b6c1a4e-2b0c-4d9f-9a1e-6f0f9b1f2a10";
        let principal = "0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";
        let sets: Vec<Vec<Label>> = vec![
            RequestOutcomeLabels {
                host: tenant.to_owned(),
                method: "GET",
                route: normalize_route(Some("/v1")),
                status: 200,
            }
            .compose(),
            StageLabels {
                host: tenant.to_owned(),
                route: normalize_route(Some("/v1")),
                phase: Phase::OutboundCall,
            }
            .compose(),
            InFlightLabels {
                host: principal.to_owned(),
            }
            .compose(),
            ErrorLabels {
                host: tenant.to_owned(),
                route: normalize_route(Some("/v1")),
                error_type: "LinkUnavailable",
            }
            .compose(),
            RateLimitLabels {
                host: tenant.to_owned(),
                path: normalize_route(Some("/v1")),
            }
            .compose(),
            TargetHostLabels {
                upstream_id: tenant.to_owned(),
                endpoint_host: principal.to_owned(),
            }
            .compose(),
            SelectionLabels {
                upstream_id: tenant.to_owned(),
                endpoint_host: principal.to_owned(),
                selection_method: "round_robin",
            }
            .compose(),
            AvailabilityLabels {
                host: tenant.to_owned(),
                endpoint: principal.to_owned(),
            }
            .compose(),
        ];
        for labels in &sets {
            for label in labels {
                assert_ne!(label.key, "tenant_id");
                assert_ne!(label.key, "subject_id");
                assert_ne!(label.key, "principal_id");
            }
        }
        // The alias-keyed sets drop an identifier-form alias: no identifier of
        // a tenant or a subject can enter a label through a value.
        for index in [0_usize, 1, 3, 4] {
            let dropped = deny_identity(sets[index].clone());
            assert!(
                dropped
                    .iter()
                    .all(|label| label.value != tenant && label.value != principal),
                "set {index} carried an identifier: {dropped:?}"
            );
        }
        // No label set has a key for a tenant, a subject or a principal, so no
        // value of one can be recorded under any instrument.
        for labels in &sets {
            assert!(
                !labels
                    .iter()
                    .any(|label| matches!(label.key, "tenant_id" | "subject_id" | "principal_id"))
            );
        }
    }

    #[test]
    fn the_identifier_labels_keep_the_domain_form_the_model_persisted() {
        // `upstream_id`, `endpoint_host` and `endpoint` carry the value the
        // domain model persisted (`inst-ob-36`), identifier form included.
        let id = "0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";
        let kept = TargetHostLabels {
            upstream_id: id.to_owned(),
            endpoint_host: "upstream.internal".to_owned(),
        }
        .compose();
        assert_eq!(deny_identity(kept).len(), 2);
    }

    #[test]
    fn every_label_set_is_composed_in_the_fixed_key_order_of_the_dod() {
        let request = RequestOutcomeLabels {
            host: "payments".to_owned(),
            method: "GET",
            route: normalize_route(Some("/v1")),
            status: 200,
        }
        .compose();
        assert_eq!(
            request.iter().map(|label| label.key).collect::<Vec<_>>(),
            [
                LABEL_HOST,
                LABEL_HTTP_REQUEST_METHOD,
                LABEL_HTTP_ROUTE,
                LABEL_HTTP_RESPONSE_STATUS_CODE,
            ]
        );
        let stage = StageLabels {
            host: "payments".to_owned(),
            route: normalize_route(Some("/v1")),
            phase: Phase::ResponsePassthrough,
        }
        .compose();
        assert_eq!(
            stage.iter().map(|label| label.key).collect::<Vec<_>>(),
            [LABEL_HOST, LABEL_HTTP_ROUTE, LABEL_PHASE]
        );
        let selection = SelectionLabels {
            upstream_id: "0a1b".to_owned(),
            endpoint_host: "upstream.internal".to_owned(),
            selection_method: "explicit_header",
        }
        .compose();
        assert_eq!(
            selection.iter().map(|label| label.key).collect::<Vec<_>>(),
            [
                LABEL_UPSTREAM_ID,
                LABEL_ENDPOINT_HOST,
                LABEL_SELECTION_METHOD,
            ]
        );
    }

    #[test]
    fn the_usage_ratio_is_clamped_to_the_unit_interval() {
        assert_eq!(usage_ratio(0, 0), 0.0);
        assert_eq!(usage_ratio(-10, 0), 0.0);
        assert_eq!(usage_ratio(10, 10), 0.0);
        assert_eq!(usage_ratio(10, 0), 1.0);
        assert_eq!(usage_ratio(10, 5), 0.5);
        // A remaining value below zero can never drive the ratio past 1.0.
        assert_eq!(usage_ratio(10, -10), 1.0);
    }

    // ------------------------------------------------------------------
    // The audit record
    // ------------------------------------------------------------------

    fn event(level: AuditLevel) -> AuditEvent {
        AuditEvent {
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            level,
            event: "proxy_request".to_owned(),
            request_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_owned()),
            tenant_id: Some("0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d".to_owned()),
            principal_id: Some("0b1c2d3e-4f5a-4b6c-8d9e-0f1a2b3c4d5e".to_owned()),
            host: "payments".to_owned(),
            path: "/oagw/v1/proxy/payments/v1/pay".to_owned(),
            method: "POST".to_owned(),
            status: 200,
            duration_ms: 12,
            request_size: 3,
            response_size: 7,
            error_type: None,
            error_message: None,
        }
    }

    #[test]
    fn the_record_line_carries_the_14_fields_in_the_design_order() {
        let line = event(AuditLevel::Info).to_json_line();
        // The members are emitted in exactly the order the ADR fixes, with the
        // 14 base keys present and nothing outside the closed set.
        assert_eq!(
            line,
            "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"level\":\"INFO\",".to_owned()
                + "\"event\":\"proxy_request\","
                + "\"request_id\":\"4bf92f3577b34da6a3ce929d0e0e4736\","
                + "\"tenant_id\":\"0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d\","
                + "\"principal_id\":\"0b1c2d3e-4f5a-4b6c-8d9e-0f1a2b3c4d5e\","
                + "\"host\":\"payments\","
                + "\"path\":\"/oagw/v1/proxy/payments/v1/pay\","
                + "\"method\":\"POST\",\"status\":200,"
                + "\"duration_ms\":12,\"request_size\":3,\"response_size\":7,"
                + "\"error_type\":null}"
        );
    }

    #[test]
    fn a_successful_record_carries_error_type_null_and_no_error_message() {
        let line = event(AuditLevel::Info).to_json_line();
        assert!(line.contains("\"error_type\":null"));
        assert!(!line.contains("error_message"));
    }

    #[test]
    fn a_failed_record_carries_the_mapped_row_and_its_message() {
        let mut failed = event(AuditLevel::Error);
        failed.error_type = Some("LinkUnavailable".to_owned());
        failed.error_message = Some("Link unavailable: Upstream link unavailable".to_owned());
        let line = failed.to_json_line();
        assert!(line.contains("\"error_type\":\"LinkUnavailable\""));
        assert!(line.contains("\"error_message\":\"Link unavailable: Upstream link unavailable\""));
    }

    #[test]
    fn the_error_message_is_the_row_fixed_text_and_never_occurrence_content() {
        let row = AuditErrorRow::of("SecretNotFound", "Secret not found");
        assert_eq!(
            row.error_message,
            "Secret not found: Referenced secret not found"
        );
        // No occurrence content — a `cred://` reference the resolution failure
        // named — is carried, so the redaction rule cannot be reached by it.
        assert!(!row.error_message.contains("cred://"));
    }

    #[test]
    fn the_error_message_is_byte_identical_for_identical_failures() {
        // The message the row carries is the row's own fixed text, so the
        // occurrence detail the producing feature composed for it never
        // reaches the record: the two occurrences below name different
        // occurrence content and still render the same message.
        let first = AuditErrorRow::of(
            "ConnectionTimeout",
            OagwError::connection_timeout("oagw.proxy: the request timeout elapsed")
                .mapping()
                .title,
        );
        let second = AuditErrorRow::of(
            "ConnectionTimeout",
            OagwError::connection_timeout("oagw.credstore: cred://missing-key")
                .mapping()
                .title,
        );
        assert_eq!(first.error_message, second.error_message);
        assert_eq!(
            first.error_message,
            "Connection timeout: Connection timed out"
        );
        assert_eq!(first.error_type, "ConnectionTimeout");
        // A `cred://` reference an occurrence names cannot reach the record,
        // because the occurrence content is not part of the message at all.
        assert!(!first.error_message.contains("cred://"));
        // The same failure rendered twice produces the same bytes.
        let mut left = event(AuditLevel::Error);
        let mut right = event(AuditLevel::Error);
        left.error_type = Some(first.error_type.to_owned());
        left.error_message = Some(first.error_message.clone());
        right.error_type = Some(second.error_type.to_owned());
        right.error_message = Some(second.error_message);
        assert_eq!(left.to_json_line(), right.to_json_line());
    }

    #[test]
    fn the_levels_are_info_warn_and_error_and_never_debug() {
        assert_eq!(AuditLevel::Info.as_str(), "INFO");
        assert_eq!(AuditLevel::Warn.as_str(), "WARN");
        assert_eq!(AuditLevel::Error.as_str(), "ERROR");
        // INFO for a success and for a management change, WARN for the
        // rate-limit refusal, ERROR for an upstream failure, a timeout and an
        // authentication failure (inst-ob-46).
        assert_eq!(AuditEventClass::RequestSuccess.level(), AuditLevel::Info);
        assert_eq!(
            AuditEventClass::ManagementChange {
                operation: "create",
                resource: "upstream",
            }
            .level(),
            AuditLevel::Info
        );
        assert_eq!(
            AuditEventClass::RateLimitRefusal {
                retry_after_secs: 3
            }
            .level(),
            AuditLevel::Warn
        );
        assert_eq!(
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            }
            .level(),
            AuditLevel::Error
        );
        assert_eq!(
            AuditEventClass::AuthenticationFailure.level(),
            AuditLevel::Error
        );
    }

    #[test]
    fn the_event_names_name_the_class_the_record_was_produced_from() {
        assert_eq!(
            AuditEventClass::ManagementChange {
                operation: "replace",
                resource: "route",
            }
            .event_name(),
            "replace_route"
        );
        assert_eq!(
            AuditEventClass::AuthenticationFailure.event_name(),
            "auth_failure"
        );
        assert_eq!(
            AuditEventClass::RateLimitRefusal {
                retry_after_secs: 1
            }
            .event_name(),
            "rate_limit_refusal"
        );
        assert_eq!(
            AuditEventClass::RequestSuccess.event_name(),
            "proxy_request"
        );
        assert_eq!(
            AuditEventClass::RequestFailure {
                passed_through_status: Some(500),
            }
            .event_name(),
            "proxy_request"
        );
    }

    #[test]
    fn only_the_successful_class_is_sampled_and_every_other_is_unsampled() {
        assert!(AuditEventClass::RequestSuccess.is_sampled());
        assert!(
            !AuditEventClass::RequestFailure {
                passed_through_status: None,
            }
            .is_sampled()
        );
        assert!(
            !AuditEventClass::RateLimitRefusal {
                retry_after_secs: 1
            }
            .is_sampled()
        );
        assert!(!AuditEventClass::AuthenticationFailure.is_sampled());
        assert!(
            !AuditEventClass::ManagementChange {
                operation: "delete",
                resource: "plugin",
            }
            .is_sampled()
        );
    }

    #[test]
    fn only_the_authentication_class_is_rate_limited() {
        assert!(AuditEventClass::AuthenticationFailure.is_rate_limited());
        assert!(!AuditEventClass::RequestSuccess.is_rate_limited());
        assert!(
            !AuditEventClass::ManagementChange {
                operation: "create",
                resource: "plugin",
            }
            .is_rate_limited()
        );
    }

    #[test]
    fn a_refusal_renders_the_row_of_the_table_and_the_delay_the_check_computed() {
        // inst-ob-21: the refusal is recorded with `error_type`
        // `RateLimitExceeded` and its `Retry-After` delay, the field set being
        // closed so the delay is carried in the row's own fixed text.
        let rendered = render(
            event(AuditLevel::Warn),
            &AuditEventClass::RateLimitRefusal {
                retry_after_secs: 7,
            },
            &SamplingCounters::new(),
            Instant::now(),
        );
        let AuditOutcome::Rendered(record) = rendered else {
            panic!("a refusal is written unsampled");
        };
        assert_eq!(record.error_type.as_deref(), Some("RateLimitExceeded"));
        let message = record
            .error_message
            .as_deref()
            .unwrap_or_else(|| panic!("a refusal carries the row of the table"));
        // The row's own fixed text, and no occurrence content beside it.
        assert!(message.contains("Rate limit exceeded"), "{message}");
        assert!(message.ends_with(" retry_after_secs=7"), "{message}");
        let line = record.to_json_line();
        assert!(
            line.contains("\"error_type\":\"RateLimitExceeded\""),
            "{line}"
        );
        assert!(line.contains("retry_after_secs=7"), "{line}");
    }

    #[test]
    fn a_refusal_renders_the_same_row_for_two_occurrences_of_one_refusal() {
        // The delay is the only occurrence-specific value a refusal carries, and
        // it arrives from the check that computed it, so two refusals of one
        // delay render byte-identical messages however they are produced.
        let first = refusal_error_row(3);
        let second = refusal_error_row(3);
        assert_eq!(first.error_type, "RateLimitExceeded");
        assert_eq!(first.error_message, second.error_message);
        // A different delay renders a different message, the check's own value.
        assert_ne!(first.error_message, refusal_error_row(4).error_message);
    }

    #[test]
    fn an_authentication_flood_from_one_tenant_leaves_another_tenants_budget_whole() {
        // The unkeyed window a record with no identity at all is admitted under
        // is the empty key, which is what `authentication_key` falls back to.
        assert_eq!(UNKEYED_WINDOW, "");
        let counters = SamplingCounters::new();
        let start = Instant::now();
        // The record of the tenant whose budget is spent, and the record of a
        // second tenant arriving in the very same second.
        let mut first = event(AuditLevel::Error);
        first.tenant_id = Some("0b6c1a4e-2b0c-4d9f-9a1e-6f0f9b1f2a10".to_owned());
        let mut second = event(AuditLevel::Error);
        second.tenant_id = Some("0c7d2b5f-3c1d-4e0a-0b2f-7a1a0c2a3b11".to_owned());
        for step in 0..AUTH_AUDIT_PER_SEC {
            assert!(matches!(
                render(
                    first.clone(),
                    &AuditEventClass::AuthenticationFailure,
                    &counters,
                    start + Duration::from_millis(u64::from(step) * 10),
                ),
                AuditOutcome::Rendered(_)
            ));
        }
        // The first tenant's budget is spent and its next record is suppressed.
        assert!(matches!(
            render(
                first,
                &AuditEventClass::AuthenticationFailure,
                &counters,
                start + Duration::from_millis(500),
            ),
            AuditOutcome::Suppressed
        ));
        // The second tenant's budget is untouched by that flood.
        assert!(matches!(
            render(
                second,
                &AuditEventClass::AuthenticationFailure,
                &counters,
                start + Duration::from_millis(500),
            ),
            AuditOutcome::Rendered(_)
        ));
    }

    #[test]
    fn a_successful_request_is_sampled_one_in_a_hundred() {
        let counters = SamplingCounters::new();
        let mut written = 0;
        for _ in 0..SUCCESS_SAMPLE * 2 {
            if matches!(
                render(
                    event(AuditLevel::Info),
                    &AuditEventClass::RequestSuccess,
                    &counters,
                    Instant::now(),
                ),
                AuditOutcome::Rendered(_)
            ) {
                written += 1;
            }
        }
        assert_eq!(
            written, 2,
            "one record in {SUCCESS_SAMPLE} successes is written"
        );
    }

    #[test]
    fn a_failed_request_a_refusal_and_a_configuration_change_are_never_sampled() {
        let counters = SamplingCounters::new();
        for _ in 0..SUCCESS_SAMPLE * 3 {
            assert!(matches!(
                render(
                    event(AuditLevel::Error),
                    &AuditEventClass::RequestFailure {
                        passed_through_status: None,
                    },
                    &counters,
                    Instant::now(),
                ),
                AuditOutcome::Rendered(_)
            ));
            assert!(matches!(
                render(
                    event(AuditLevel::Warn),
                    &AuditEventClass::RateLimitRefusal {
                        retry_after_secs: 2
                    },
                    &counters,
                    Instant::now(),
                ),
                AuditOutcome::Rendered(_)
            ));
            assert!(matches!(
                render(
                    event(AuditLevel::Info),
                    &AuditEventClass::ManagementChange {
                        operation: "create",
                        resource: "upstream",
                    },
                    &counters,
                    Instant::now(),
                ),
                AuditOutcome::Rendered(_)
            ));
        }
    }

    #[test]
    fn the_authentication_class_is_rate_limited_to_five_records_per_second() {
        let counters = SamplingCounters::new();
        let start = Instant::now();
        let mut written = 0;
        for step in 0..AUTH_AUDIT_PER_SEC * 4 {
            let admitted = matches!(
                render(
                    event(AuditLevel::Error),
                    &AuditEventClass::AuthenticationFailure,
                    &counters,
                    start + Duration::from_millis(u64::from(step) * 10),
                ),
                AuditOutcome::Rendered(_)
            );
            if admitted {
                written += 1;
            }
        }
        // Twenty attempts within the sliding one-second window admit at most
        // `AUTH_AUDIT_PER_SEC` of them.
        assert_eq!(written, AUTH_AUDIT_PER_SEC);
        // And the window slides: a failure a second later is admitted again.
        assert!(matches!(
            render(
                event(AuditLevel::Error),
                &AuditEventClass::AuthenticationFailure,
                &counters,
                start + Duration::from_secs(2),
            ),
            AuditOutcome::Rendered(_)
        ));
    }

    #[test]
    fn a_record_a_redaction_rule_touches_is_suppressed_whole() {
        // No query string, no header value and no credential material reaches a
        // field: a value carrying one is dropped whole, never written short a
        // field (inst-ob-40, inst-ob-42).
        let mut carrying = event(AuditLevel::Warn);
        carrying.path = "/oagw/v1/proxy/payments/v1/pay?token=secret".to_owned();
        carrying.error_type = Some("RateLimitExceeded".to_owned());
        assert!(matches!(
            render(
                carrying,
                &AuditEventClass::RateLimitRefusal {
                    retry_after_secs: 1
                },
                &SamplingCounters::new(),
                Instant::now(),
            ),
            AuditOutcome::Suppressed
        ));
        let mut credentialed = event(AuditLevel::Error);
        credentialed.error_message = Some("Downstream error: bearer abc.def".to_owned());
        credentialed.error_type = Some("DownstreamError".to_owned());
        assert!(matches!(
            render(
                credentialed,
                &AuditEventClass::RequestFailure {
                    passed_through_status: None,
                },
                &SamplingCounters::new(),
                Instant::now(),
            ),
            AuditOutcome::Suppressed
        ));
        // A record that carries none of it is written whole.
        assert!(matches!(
            render(
                event(AuditLevel::Error),
                &AuditEventClass::RequestFailure {
                    passed_through_status: None,
                },
                &SamplingCounters::new(),
                Instant::now(),
            ),
            AuditOutcome::Rendered(_)
        ));
    }

    #[test]
    fn the_audit_path_is_gear_relative_with_no_query_string() {
        assert_eq!(
            audit_path("payments", "/v1/pay"),
            "/oagw/v1/proxy/payments/v1/pay"
        );
        assert_eq!(
            audit_path("payments", "/v1/pay?token=secret"),
            "/oagw/v1/proxy/payments/v1/pay"
        );
        assert_eq!(
            audit_path("payments", "/v1/pay#fragment"),
            "/oagw/v1/proxy/payments/v1/pay"
        );
        assert_eq!(audit_path("payments", "/"), "/oagw/v1/proxy/payments");
        // Never the /api-prefixed alias form.
        assert!(!audit_path("payments", "/api/v1/pay").starts_with("/api"));
    }

    #[test]
    fn the_management_path_is_gear_relative_with_no_query_string() {
        assert_eq!(
            audit_management_path("/oagw/v1/upstreams"),
            "/oagw/v1/upstreams"
        );
        assert_eq!(
            audit_management_path("/oagw/v1/upstreams?top=5"),
            "/oagw/v1/upstreams"
        );
    }

    // ------------------------------------------------------------------
    // The record state machine
    // ------------------------------------------------------------------

    #[test]
    fn the_state_machine_declares_exactly_the_four_transitions() {
        assert_eq!(
            AuditRecordState::Collected.transition(AuditRecordState::Rendered),
            Some(AuditRecordState::Rendered)
        );
        assert_eq!(
            AuditRecordState::Collected.transition(AuditRecordState::Suppressed),
            Some(AuditRecordState::Suppressed)
        );
        assert_eq!(
            AuditRecordState::Rendered.transition(AuditRecordState::Written),
            Some(AuditRecordState::Written)
        );
        assert_eq!(
            AuditRecordState::Rendered.transition(AuditRecordState::Suppressed),
            Some(AuditRecordState::Suppressed)
        );
    }

    #[test]
    fn the_state_machine_refuses_every_other_transition() {
        for from in [
            AuditRecordState::Collected,
            AuditRecordState::Rendered,
            AuditRecordState::Suppressed,
            AuditRecordState::Written,
        ] {
            for to in [
                AuditRecordState::Collected,
                AuditRecordState::Rendered,
                AuditRecordState::Suppressed,
                AuditRecordState::Written,
            ] {
                let declared = matches!(
                    (from, to),
                    (AuditRecordState::Collected, AuditRecordState::Rendered)
                        | (AuditRecordState::Collected, AuditRecordState::Suppressed)
                        | (AuditRecordState::Rendered, AuditRecordState::Written)
                        | (AuditRecordState::Rendered, AuditRecordState::Suppressed)
                );
                assert_eq!(
                    from.transition(to).is_some(),
                    declared,
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    /// The member names a rendered JSON line carries, in the order written.
    fn members_of(line: &str) -> Vec<&str> {
        line.trim_start_matches('{')
            .trim_end_matches('}')
            .split(',')
            .map(|member| {
                member
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .trim_matches('"')
            })
            .collect()
    }

    #[test]
    fn every_event_class_renders_the_closed_field_set_and_nothing_outside_it() {
        const BASE_FIELDS: [&str; 14] = [
            "timestamp",
            "level",
            "event",
            "request_id",
            "tenant_id",
            "principal_id",
            "host",
            "path",
            "method",
            "status",
            "duration_ms",
            "request_size",
            "response_size",
            "error_type",
        ];
        let classes = [
            AuditEventClass::ManagementChange {
                operation: "create",
                resource: "upstream",
            },
            AuditEventClass::AuthenticationFailure,
            AuditEventClass::RateLimitRefusal {
                retry_after_secs: 3,
            },
            AuditEventClass::RequestFailure {
                passed_through_status: Some(502),
            },
            AuditEventClass::RequestSuccess,
        ];
        for class in &classes {
            let mut record = event(class.level());
            record.event = class.event_name();
            if class.is_failed_request() {
                record.error_type = Some("StreamAborted".to_owned());
                record.error_message = Some("Stream aborted: Stream connection aborted".to_owned());
            }
            let counters = SamplingCounters::new();
            let mut line = None;
            for _ in 0..=SUCCESS_SAMPLE {
                if let AuditOutcome::Rendered(rendered) =
                    render(record.clone(), class, &counters, Instant::now())
                {
                    line = Some(rendered.to_json_line());
                    break;
                }
            }
            let line = line.unwrap_or_else(|| panic!("the class is written unsampled"));
            let mut carried = members_of(&line);
            carried.sort_unstable();
            let mut expected = BASE_FIELDS.to_vec();
            // `error_message` is present on a failed request's record and on the
            // refusal's, the row of the closed table a refusal is rendered
            // through carrying the `Retry-After` delay the check computed
            // (`inst-ob-21`) because no field of its own exists for it.
            if class.is_failed_request()
                || matches!(class, AuditEventClass::RateLimitRefusal { .. })
            {
                expected.push("error_message");
            }
            expected.sort_unstable();
            assert_eq!(
                carried, expected,
                "the field set of {class:?} is closed: {line}"
            );
        }
    }

    #[test]
    fn the_audit_class_vocabulary_names_no_circuit_breaker_event() {
        // The array below is exhaustive over the class vocabulary, so a
        // circuit-breaker class added to it would not compile: no class the
        // vocabulary fixes opens a record whose event names a circuit breaker,
        // and no circuit-breaker audit event is emitted for anything.
        let classes = [
            AuditEventClass::ManagementChange {
                operation: "create",
                resource: "plugin",
            },
            AuditEventClass::AuthenticationFailure,
            AuditEventClass::RateLimitRefusal {
                retry_after_secs: 1,
            },
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
            AuditEventClass::RequestSuccess,
        ];
        for class in &classes {
            let name = class.event_name();
            assert!(
                !name.contains("circuit"),
                "no circuit-breaker audit event exists: {name}"
            );
        }
    }

    #[test]
    fn a_sampled_out_success_is_suppressed_before_it_is_rendered() {
        // The sampling gate runs inside the filter, so the sampled-out record
        // is dropped without ever reaching the rendered state: the first
        // SUCCESS_SAMPLE - 1 successes are dropped and the SUCCESS_SAMPLE-th
        // one is the only record of the batch.
        let counters = SamplingCounters::new();
        for _ in 0..SUCCESS_SAMPLE - 1 {
            assert!(matches!(
                render(
                    event(AuditLevel::Info),
                    &AuditEventClass::RequestSuccess,
                    &counters,
                    Instant::now(),
                ),
                AuditOutcome::Suppressed
            ));
        }
        let last = render(
            event(AuditLevel::Info),
            &AuditEventClass::RequestSuccess,
            &counters,
            Instant::now(),
        );
        assert!(matches!(last, AuditOutcome::Rendered(_)));
    }
}
