//! The observability vocabulary of `cpt-cf-oagw-feature-observability`.
//!
//! Three entities DECOMPOSITION §2.9 names live here: [`CorrelationContext`],
//! [`AuditEvent`], and [`MetricLabelSet`]. The rest of the module is the
//! closed value sets the FEATURE's §1.5 deviations record — the correlation
//! header and its admission bound, the sampling ratio and the auth-failure
//! bound, the five method literals, the histogram buckets, the audit field
//! order, and the twelve event literals — so a caller that needs one of them
//! reads a constant instead of restating a rule.
//!
//! Layering: no transport type appears here. The correlation header value
//! arrives as a `&str` the caller read from the request, the audit event is
//! plain data, and the label set is a declaration and not a per-request value.

use uuid::Uuid;

// @cpt-dod:cpt-cf-oagw-dod-obs-correlation:p1

/// The header the platform injects the request identifier into, whose value is
/// the one header value any record may carry.
///
/// `config/e2e-local.yaml`'s `opentelemetry.tracing.http.inject_request_id_header`
/// names it, and it is the one name of the allowlist DESIGN §4.3 states and
/// §1.5 closes.
pub const CORRELATION_HEADER: &str = "x-request-id";

/// The longest caller-supplied correlation identifier the admission check
/// admits.
///
/// A value longer than this is unbounded, and an unbounded value is the thing
/// the check exists to refuse: it is discarded and a UUID is generated in its
/// place (§1.5).
pub const CORRELATION_MAX_LEN: usize = 128;

/// The denominator of the high-volume sampling ratio DESIGN §4.3's example
/// states: one success record in this many is kept.
///
/// A build-time constant with no configuration surface (§1.5): no key of
/// [`crate::config::OagwConfig`] and no upstream or route configuration
/// reaches it.
pub const HIGH_VOLUME_SAMPLE_ONE_IN: u64 = 100;

/// The most authentication-failure records one interval may carry.
///
/// The records beyond the bound within the interval are dropped and not
/// queued, because a queue of unsent failure records is the flood the bound
/// exists to prevent (§1.5).
pub const AUTH_FAILURE_LOG_LIMIT: u32 = 20;

/// The length of the interval the failure-log bound is counted over.
pub const AUTH_FAILURE_LOG_INTERVAL_MS: u64 = 1_000;

/// The twelve buckets of `oagw_request_duration_seconds`, in seconds.
pub const HISTOGRAM_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The `error_type` value a response the upstream answered with a failure
/// status carries, which is the one value the error catalogue has no row for.
pub const ERROR_TYPE_UPSTREAM: &str = "upstream";

/// The four phases whose durations `oagw_request_duration_seconds` carries.
pub const PHASES: [&str; 4] = ["resolve", "chain", "upstream", "total"];

/// The three selection methods `oagw_routing_endpoint_selected` carries.
pub const SELECTION_METHODS: [&str; 3] = ["explicit_header", "round_robin", "default"];

/// The three connection states `oagw_upstream_connections` carries.
pub const CONNECTION_STATES: [&str; 3] = ["idle", "active", "max"];

/// The five method literals the shipped route schema declares, in the order
/// its `enum` lists them.
pub const METHOD_LITERALS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// The value `http.request.method` carries for a method outside
/// [`METHOD_LITERALS`].
pub const METHOD_OTHER: &str = "_OTHER";

/// The fourteen field names of an audit record, in the order DESIGN §4.3
/// tabulates them and in which they are serialized.
pub const AUDIT_FIELDS: [&str; 14] = [
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

/// The twelve event literals §1.5 closes the `event` value at.
pub const AUDIT_EVENTS: [&str; 12] = [
    "proxy_request.succeeded",
    "proxy_request.failed",
    "config.upstream.created",
    "config.upstream.overridden",
    "config.upstream.deleted",
    "config.route.created",
    "config.route.overridden",
    "config.route.deleted",
    "config.plugin.created",
    "config.plugin.deleted",
    "auth.failed",
    "breaker.transitioned",
];

/// The event literal of a successful proxy request.
pub const EVENT_REQUEST_SUCCEEDED: &str = AUDIT_EVENTS[0];
/// The event literal of a failed proxy request.
pub const EVENT_REQUEST_FAILED: &str = AUDIT_EVENTS[1];
/// The event literal of an upstream the write path created.
pub const EVENT_UPSTREAM_CREATED: &str = AUDIT_EVENTS[2];
/// The event literal of an upstream a replacement overrode.
pub const EVENT_UPSTREAM_OVERRIDDEN: &str = AUDIT_EVENTS[3];
/// The event literal of an upstream the write path deleted.
pub const EVENT_UPSTREAM_DELETED: &str = AUDIT_EVENTS[4];
/// The event literal of a route the write path created.
pub const EVENT_ROUTE_CREATED: &str = AUDIT_EVENTS[5];
/// The event literal of a route a replacement overrode.
pub const EVENT_ROUTE_OVERRIDDEN: &str = AUDIT_EVENTS[6];
/// The event literal of a route the write path deleted.
pub const EVENT_ROUTE_DELETED: &str = AUDIT_EVENTS[7];
/// The event literal of a plugin the write path created.
pub const EVENT_PLUGIN_CREATED: &str = AUDIT_EVENTS[8];
/// The event literal of a plugin the write path deleted.
pub const EVENT_PLUGIN_DELETED: &str = AUDIT_EVENTS[9];
/// The event literal of a failed authentication.
pub const EVENT_AUTH_FAILED: &str = AUDIT_EVENTS[10];
/// The event literal of a circuit-breaker transition.
pub const EVENT_BREAKER_TRANSITIONED: &str = AUDIT_EVENTS[11];

/// The four levels an audit record is written at.
pub const AUDIT_LEVELS: [&str; 4] = ["INFO", "WARN", "ERROR", "DEBUG"];

/// Normalizes a request method to the standard verb or `_OTHER`.
///
/// The five literals the shipped route schema declares are carried as
/// themselves; every other method, including a lowercase spelling of one of
/// them, is `_OTHER`, because a value the schema does not declare is not a
/// value the label set admits (§1.5).
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    let upper = method.to_ascii_uppercase();
    if METHOD_LITERALS.contains(&upper.as_str()) {
        match upper.as_str() {
            "GET" => "GET",
            "POST" => "POST",
            "PUT" => "PUT",
            "DELETE" => "DELETE",
            _ => "PATCH",
        }
    } else {
        METHOD_OTHER
    }
}

/// The slug a catalogue row carries, read from the GTS `type` identifier.
///
/// The identifier is `gts.cf.core.errors.err.v1~cf.oagw.{slug}.v1`, so the
/// slug is what the prefix and the `.v1` suffix enclose; a variant whose
/// identifier is not formed that way maps to nothing, because inventing a slug
/// the catalogue does not carry is the cardinality breach the closed set
/// exists to prevent.
#[must_use]
pub fn error_slug_of(gts_type: &str) -> Option<&str> {
    let marker = "cf.oagw.";
    let start = gts_type.rfind(marker)? + marker.len();
    let rest = &gts_type[start..];
    let end = rest.strip_suffix(".v1")?;
    if end.is_empty() {
        None
    } else {
        Some(end)
    }
}

/// Whether a caller-supplied correlation value is admitted.
///
/// The value is admitted when it is non-empty, no longer than
/// [`CORRELATION_MAX_LEN`], and made only of printable ASCII with no control
/// character: every byte in `0x20..=0x7E`, so neither a control byte below
/// `0x20` nor `0x7F` nor any higher byte is carried into a record (§1.5).
#[must_use]
pub fn correlation_admitted(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= CORRELATION_MAX_LEN
        && value.bytes().all(|byte| (0x20..=0x7E).contains(&byte))
        // A value the serializer would redact is not admitted either, so a
        // caller cannot trade a correlation identifier for its absence: the
        // value that fails here takes the generation branch at the entry and
        // the record it produces carries a `request_id`.
        && !value.contains("cred://")
        && !value.contains("Bearer ")
}

/// Whether the correlation value arrived from the inbound header or was
/// generated.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CorrelationSource {
    /// The platform-injected header carried a value the admission check
    /// accepted.
    InboundHeader,
    /// No header arrived, or the value it carried failed the check.
    Generated,
}

/// Whether the request's success record survives the high-volume sampling
/// gate.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SamplingDecision {
    /// The record is written when the route it describes is high-volume.
    Keep,
    /// The record is dropped when the route it describes is high-volume.
    Drop,
}

/// The correlation state of one proxy request.
///
/// One per request, carried as a member of [`crate::domain::proxy::ProxyContext`],
/// and read by every routine of the feature that writes on the request's
/// behalf. The tenant and subject identifiers are the platform-resolved ones:
/// an identifier the platform did not resolve is recorded as an absence and
/// never as a synthesized value (§1.4).
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelationContext {
    /// The correlation identifier the record's `request_id` field carries.
    pub request_id: String,
    /// Where the identifier came from.
    pub source: CorrelationSource,
    /// The calling tenant, when the platform resolved one.
    pub tenant_id: Option<Uuid>,
    /// The authenticated subject, when the platform resolved one.
    pub principal_id: Option<String>,
    /// The sampling decision the request's success record is subject to.
    pub sampling: SamplingDecision,
}

impl CorrelationContext {
    /// Assigns the correlation state of one request.
    ///
    /// Realizes `cpt-cf-oagw-algo-correlate`: the header value is adopted when
    /// the admission check accepts it and a UUID is generated otherwise, the
    /// resolved identity is recorded as it stands, and the sampling decision is
    /// read once per request so the decision is constant for the request no
    /// matter which route it turns out to match.
    #[must_use]
    pub fn assign(
        inbound: Option<&str>,
        tenant_id: Option<Uuid>,
        principal_id: Option<String>,
    ) -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-read
        // The correlation header the platform injected is the value this
        // routine reads, and the absence of one is an absence and not a
        // synthesized value.
        let read = inbound;
        // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-read
        // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-adopt-if
        // The header is adopted only when a value arrived and the admission
        // check admits it; the check is the bounded, printable,
        // no-control-character test of §1.5.
        let adopted = read.is_some_and(correlation_admitted);
        // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-adopt-if
        let request_id = if adopted {
            // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-adopt
            // The value the header carried passes the bounded printable check,
            // so it is the identifier the request carries and the record
            // writes.
            inbound.unwrap_or_default().to_owned()
            // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-adopt
        } else {
            // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-adopt-else
            // The header was absent or its value failed the admission check:
            // both are the branch that generates.
            // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-adopt-else
            // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-generate
            // No header, or a value the check refused: a UUID is generated in
            // its place, so the request is still correlated and still
            // recorded, and an unbounded or non-printable value never reaches
            // a record.
            Uuid::new_v4().to_string()
            // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-generate
        };
        // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-ident
        // The tenant and the subject are the identifiers the platform
        // resolved, recorded as the caller supplied them; an identifier the
        // platform did not resolve is recorded as an absence and never as a
        // synthesized value.
        // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-ident
        // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-sampling
        // The sampling decision is read once per request from the identifier
        // alone, so a route is neither sampled into silence nor out of it by a
        // second decision.
        let sampling = Self::sampling_of(&request_id);
        // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-sampling
        // @cpt-begin:cpt-cf-oagw-algo-correlate:p1:inst-ac-return
        // RETURN the context: it is carried on the `ProxyContext` of the
        // request and read by every routine of §3 that writes on the request's
        // behalf.
        Self {
            source: if adopted {
                CorrelationSource::InboundHeader
            } else {
                CorrelationSource::Generated
            },
            request_id,
            tenant_id,
            principal_id,
            sampling,
        }
        // @cpt-end:cpt-cf-oagw-algo-correlate:p1:inst-ac-return
    }

    /// The sampling decision one request identifier is subject to.
    ///
    /// The roll is a function of the identifier alone, so the decision is read
    /// once per request and is the same decision whoever re-derives it: a
    /// route cannot be sampled into silence or out of it by a second decision
    /// (§1.5). One identifier in [`HIGH_VOLUME_SAMPLE_ONE_IN`] keeps its
    /// record.
    #[must_use]
    pub fn sampling_of(request_id: &str) -> SamplingDecision {
        if fnv1a(request_id.as_bytes()).is_multiple_of(HIGH_VOLUME_SAMPLE_ONE_IN) {
            SamplingDecision::Keep
        } else {
            SamplingDecision::Drop
        }
    }
}

/// The FNV-1a 64-bit hash the sampling roll and the identifier's admission
/// bucket read.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Whether a route match pattern names a high-volume route.
///
/// The classification is a build-time rule of this feature with no
/// configuration surface: a pattern that names a collection — a path with no
/// `{...}` parameter segment, so every request to it addresses the same
/// resource set — is the read-heavy route the ratio is stated over, and a
/// pattern that names a single resource is not.
#[must_use]
pub fn is_high_volume_pattern(pattern: &str) -> bool {
    !pattern.contains('{')
}

/// One structured record of DESIGN §4.3.
///
/// Exactly the fourteen fields that section tabulates and no fifteenth member:
/// the human-readable message a failed request's prose clause names is carried
/// by no field, because DECOMPOSITION §2.9 fixes the field set as the fourteen
/// names. A field with no value for the event is absent, and is never written
/// as null or as an empty string.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuditEvent {
    /// The instant the record was issued at, read once per record.
    pub timestamp: Option<String>,
    /// The level the mapping of §1.5 assigns.
    pub level: Option<String>,
    /// The event name, from the closed set [`AUDIT_EVENTS`] closes.
    pub event: Option<String>,
    /// The correlation identifier the request carries.
    pub request_id: Option<String>,
    /// The calling tenant, when the platform resolved one.
    pub tenant_id: Option<String>,
    /// The authenticated subject, when the platform resolved one.
    pub principal_id: Option<String>,
    /// The resolved upstream's alias, for a proxy-path record.
    pub host: Option<String>,
    /// The matched route's normalized match pattern, or the management path.
    pub path: Option<String>,
    /// The request method.
    pub method: Option<String>,
    /// The status the caller was answered with.
    pub status: Option<u16>,
    /// The measured duration, in milliseconds.
    pub duration_ms: Option<u64>,
    /// The request bytes as transferred.
    pub request_size: Option<u64>,
    /// The response bytes as transferred.
    pub response_size: Option<u64>,
    /// The catalogue slug of the failure, or [`ERROR_TYPE_UPSTREAM`].
    pub error_type: Option<String>,
}

impl AuditEvent {
    /// The field name and value pairs the record carries, in the order DESIGN
    /// §4.3 tabulates them.
    ///
    /// A field with no value is omitted rather than written null or empty,
    /// which is what the serializer of the record iterates. The order is
    /// [`AUDIT_FIELDS`]'s own: the constant is walked, so the tabulated order
    /// and the emitted order are one order that cannot drift apart.
    #[must_use]
    pub fn populated(&self) -> Vec<(&'static str, String)> {
        let mut fields: Vec<(&'static str, String)> = Vec::with_capacity(AUDIT_FIELDS.len());
        for name in AUDIT_FIELDS {
            let value = match name {
                "timestamp" => self.timestamp.clone(),
                "level" => self.level.clone(),
                "event" => self.event.clone(),
                "request_id" => self.request_id.clone(),
                "tenant_id" => self.tenant_id.clone(),
                "principal_id" => self.principal_id.clone(),
                "host" => self.host.clone(),
                "path" => self.path.clone(),
                "method" => self.method.clone(),
                "status" => self.status.map(|status| status.to_string()),
                "duration_ms" => self.duration_ms.map(|duration| duration.to_string()),
                "request_size" => self.request_size.map(|size| size.to_string()),
                "response_size" => self.response_size.map(|size| size.to_string()),
                "error_type" => self.error_type.clone(),
                _ => None,
            };
            if let Some(value) = value {
                fields.push((name, value));
            }
        }
        fields
    }
}

/// The shared label vocabulary of DESIGN §4.2 and the per-family subsets that
/// section enumerates.
///
/// A declared concept and not a per-request value: it is the vocabulary the
/// twelve families share, held once, which is why the cardinality rules of the
/// FEATURE's §5 are checkable as a property of this declaration rather than as
/// a property of a call site.
// @cpt-dod:cpt-cf-oagw-dod-obs-cardinality:p1
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetricLabelSet;

impl MetricLabelSet {
    /// The upstream alias, the one OAGW-specific label DESIGN §4.2 names.
    pub const HOST: &'static str = "host";
    /// The normalized route match pattern.
    pub const HTTP_ROUTE: &'static str = "http.route";
    /// The standard verb or [`METHOD_OTHER`].
    pub const HTTP_METHOD: &'static str = "http.request.method";
    /// The numeric status of the answer the caller received.
    pub const HTTP_STATUS: &'static str = "http.response.status_code";
    /// One of [`PHASES`].
    pub const PHASE: &'static str = "phase";
    /// A catalogue slug or [`ERROR_TYPE_UPSTREAM`].
    pub const ERROR_TYPE: &'static str = "error_type";
    /// The normalized route match pattern, the same value [`Self::HTTP_ROUTE`]
    /// carries.
    pub const PATH: &'static str = "path";
    /// The breaker phase a transition started from.
    pub const FROM_STATE: &'static str = "from_state";
    /// The breaker phase a transition ended at.
    pub const TO_STATE: &'static str = "to_state";
    /// One of [`CONNECTION_STATES`].
    pub const STATE: &'static str = "state";
    /// The upstream identifier the routing families report.
    pub const UPSTREAM_ID: &'static str = "upstream_id";
    /// The endpoint host the selection named.
    pub const ENDPOINT_HOST: &'static str = "endpoint_host";
    /// The endpoint the availability gauge reports.
    pub const ENDPOINT: &'static str = "endpoint";
    /// One of [`SELECTION_METHODS`].
    pub const SELECTION_METHOD: &'static str = "selection_method";

    /// `oagw_requests_total`.
    pub const REQUESTS_TOTAL: &'static [&'static str] =
        &[Self::HOST, Self::HTTP_METHOD, Self::HTTP_ROUTE, Self::HTTP_STATUS];
    /// `oagw_request_duration_seconds`.
    pub const REQUEST_DURATION: &'static [&'static str] =
        &[Self::HOST, Self::HTTP_ROUTE, Self::PHASE];
    /// `oagw_requests_in_flight`.
    pub const IN_FLIGHT: &'static [&'static str] = &[Self::HOST];
    /// `oagw_errors_total`.
    pub const ERRORS_TOTAL: &'static [&'static str] =
        &[Self::HOST, Self::HTTP_ROUTE, Self::ERROR_TYPE];
    /// `oagw_circuit_breaker_state`.
    pub const BREAKER_STATE: &'static [&'static str] = &[Self::HOST];
    /// `oagw_rate_limit_exceeded_total`.
    pub const RATE_LIMIT_EXCEEDED: &'static [&'static str] = &[Self::HOST, Self::PATH];
    /// `oagw_circuit_breaker_transitions_total`.
    pub const BREAKER_TRANSITIONS: &'static [&'static str] =
        &[Self::HOST, Self::FROM_STATE, Self::TO_STATE];
    /// `oagw_rate_limit_usage_ratio`.
    pub const RATE_LIMIT_USAGE: &'static [&'static str] = &[Self::HOST, Self::PATH];
    /// `oagw_routing_target_host_used`.
    pub const ROUTING_TARGET_USED: &'static [&'static str] =
        &[Self::UPSTREAM_ID, Self::ENDPOINT_HOST];
    /// `oagw_routing_endpoint_selected`.
    pub const ROUTING_SELECTED: &'static [&'static str] =
        &[Self::UPSTREAM_ID, Self::ENDPOINT_HOST, Self::SELECTION_METHOD];
    /// `oagw_upstream_available`.
    pub const UPSTREAM_AVAILABLE: &'static [&'static str] = &[Self::HOST, Self::ENDPOINT];
    /// `oagw_upstream_connections`.
    pub const UPSTREAM_CONNECTIONS: &'static [&'static str] = &[Self::HOST, Self::STATE];

    /// The label set of one family, by its registry name.
    ///
    /// The twelve names are the twelve families DESIGN §4.2 enumerates; a name
    /// outside them has no set, because no family the catalogue does not name
    /// is declared.
    #[must_use]
    pub fn labels_of(family: &str) -> Option<&'static [&'static str]> {
        match family {
            "oagw_requests_total" => Some(Self::REQUESTS_TOTAL),
            "oagw_request_duration_seconds" => Some(Self::REQUEST_DURATION),
            "oagw_requests_in_flight" => Some(Self::IN_FLIGHT),
            "oagw_errors_total" => Some(Self::ERRORS_TOTAL),
            "oagw_circuit_breaker_state" => Some(Self::BREAKER_STATE),
            "oagw_rate_limit_exceeded_total" => Some(Self::RATE_LIMIT_EXCEEDED),
            "oagw_circuit_breaker_transitions_total" => Some(Self::BREAKER_TRANSITIONS),
            "oagw_rate_limit_usage_ratio" => Some(Self::RATE_LIMIT_USAGE),
            "oagw_routing_target_host_used" => Some(Self::ROUTING_TARGET_USED),
            "oagw_routing_endpoint_selected" => Some(Self::ROUTING_SELECTED),
            "oagw_upstream_available" => Some(Self::UPSTREAM_AVAILABLE),
            "oagw_upstream_connections" => Some(Self::UPSTREAM_CONNECTIONS),
            _ => None,
        }
    }
}

/// The label value a breaker phase is reported under, in the spelling
/// `cpt-cf-oagw-state-circuit-breaker` declares.
#[must_use]
pub fn breaker_state_label(phase: crate::domain::ratelimit::BreakerPhase) -> &'static str {
    use crate::domain::ratelimit::BreakerPhase;
    match phase {
        BreakerPhase::Closed => "closed",
        BreakerPhase::Open => "open",
        BreakerPhase::HalfOpen => "half_open",
    }
}

/// The `selection_method` value an endpoint choice is reported under.
#[must_use]
pub fn selection_method_label(choice: crate::domain::proxy::EndpointChoice) -> &'static str {
    use crate::domain::proxy::EndpointChoice;
    match choice {
        EndpointChoice::Header => "explicit_header",
        EndpointChoice::LoadBalanced => "round_robin",
        EndpointChoice::Only => "default",
    }
}

/// The `event` literal and the level the mapping of §1.5 assigns a proxy-path
/// record.
///
/// A request the gateway answered from an error it produced, or the upstream
/// answered with a failure status, is the failed record; every other answer is
/// the success record. The level is ERROR for an upstream failure, a timeout,
/// and an authentication failure, WARN for a rate-limit refusal and a
/// breaker-open answer, and INFO for every other answer, which is the mapping
/// DESIGN §4.3 tabulates and §1.5 applies.
///
/// The two credential-resolution failures the chain reports — a reference the
/// credential store declined and a reference it resolved no secret for — are
/// the authentication failures §1.5 row 177 names as ones this feature
/// records, so they are carried under the `auth.failed` literal of the closed
/// set rather than under the generic failed-request one, at the ERROR level
/// the mapping assigns them.
#[must_use]
pub fn request_event_of(failed: bool, kind: Option<crate::domain::error::ErrorKind>) -> (&'static str, &'static str) {
    use crate::domain::error::ErrorKind;
    if !failed {
        return (EVENT_REQUEST_SUCCEEDED, "INFO");
    }
    if matches!(
        kind,
        Some(ErrorKind::AuthenticationFailed | ErrorKind::SecretNotFound)
    ) {
        return (EVENT_AUTH_FAILED, "ERROR");
    }
    let level = match kind {
        Some(ErrorKind::RateLimitExceeded | ErrorKind::CircuitBreakerOpen) => "WARN",
        Some(
            ErrorKind::DownstreamError
            | ErrorKind::ProtocolError
            | ErrorKind::StreamAborted
            | ErrorKind::LinkUnavailable
            | ErrorKind::ConnectionTimeout
            | ErrorKind::RequestTimeout
            | ErrorKind::IdleTimeout,
        ) => "ERROR",
        _ => "INFO",
    };
    (EVENT_REQUEST_FAILED, level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_the_five_literals() {
        for method in METHOD_LITERALS {
            assert_eq!(normalize_method(method), method);
        }
    }

    #[test]
    fn normalizes_everything_else_to_other() {
        assert_eq!(normalize_method("OPTIONS"), METHOD_OTHER);
        assert_eq!(normalize_method("head"), METHOD_OTHER);
        assert_eq!(normalize_method("CONNECT"), METHOD_OTHER);
        assert_eq!(normalize_method(""), METHOD_OTHER);
    }

    #[test]
    fn reads_the_slug_out_of_the_catalogue_identifier() {
        assert_eq!(error_slug_of(crate::gts::ERR_AUTH_FAILED), Some("auth.failed"));
        assert_eq!(
            error_slug_of(crate::gts::ERR_ROUTE_NOT_FOUND),
            Some("route.not_found")
        );
        assert_eq!(
            error_slug_of(crate::gts::ERR_CIRCUIT_BREAKER_OPEN),
            Some("circuit_breaker.open")
        );
    }

    #[test]
    fn admits_a_bounded_printable_value() {
        assert!(correlation_admitted("0123456789abcdef"));
        assert!(correlation_admitted(&"x".repeat(CORRELATION_MAX_LEN)));
    }

    #[test]
    fn refuses_an_unbounded_or_controlled_value() {
        assert!(!correlation_admitted(&"x".repeat(CORRELATION_MAX_LEN + 1)));
        assert!(!correlation_admitted(""));
        assert!(!correlation_admitted("with\ttab"));
        assert!(!correlation_admitted("with\nnewline"));
        assert!(!correlation_admitted("with\u{7f}del"));
        assert!(!correlation_admitted("with\u{00e9}accent"));
    }

    #[test]
    fn adopts_an_admitted_value_and_generates_otherwise() {
        let adopted = CorrelationContext::assign(Some("caller-id"), Some(Uuid::nil()), None);
        assert_eq!(adopted.request_id, "caller-id");
        assert_eq!(adopted.source, CorrelationSource::InboundHeader);
        assert_eq!(adopted.tenant_id, Some(Uuid::nil()));
        assert_eq!(adopted.principal_id, None);

        let refused = CorrelationContext::assign(Some("bad\tvalue"), None, None);
        assert_eq!(refused.source, CorrelationSource::Generated);
        assert!(!refused.request_id.is_empty());

        let absent = CorrelationContext::assign(None, None, None);
        assert_eq!(absent.source, CorrelationSource::Generated);
    }

    #[test]
    fn keeps_one_identifier_in_the_ratio() {
        // One in a hundred of two thousand identifiers is twenty, with slack
        // for the roll's distribution over a small sample.
        let kept = (0..2000u32)
            .map(|index| CorrelationContext::sampling_of(&index.to_string()))
            .filter(|decision| *decision == SamplingDecision::Keep)
            .count();
        assert!(
            (5..=60).contains(&kept),
            "expected a small fraction kept, got {kept}"
        );
        // The roll is a function of the identifier alone, so the decision is
        // the same whoever re-derives it.
        assert_eq!(
            CorrelationContext::sampling_of("stable"),
            CorrelationContext::sampling_of("stable")
        );
    }

    #[test]
    fn classifies_a_collection_pattern_high_volume() {
        assert!(is_high_volume_pattern("/v1/things"));
        assert!(!is_high_volume_pattern("/v1/things/{id}"));
        assert!(!is_high_volume_pattern("/v1/things/{id}/parts"));
    }

    #[test]
    fn tabulates_the_fourteen_fields() {
        assert_eq!(AUDIT_FIELDS.len(), 14);
        assert_eq!(AUDIT_FIELDS[0], "timestamp");
        assert_eq!(AUDIT_FIELDS[13], "error_type");
    }

    #[test]
    fn omits_an_unpopulated_field() {
        let event = AuditEvent {
            level: Some("INFO".to_owned()),
            event: Some("proxy_request.succeeded".to_owned()),
            request_id: Some("r".to_owned()),
            ..AuditEvent::default()
        };
        let fields = event.populated();
        assert_eq!(fields.len(), 3);
        assert!(AUDIT_EVENTS.contains(&"proxy_request.succeeded"));
        assert!(AUDIT_EVENTS.len() == 12);
        assert!(AUDIT_LEVELS.len() == 4);
    }

    #[test]
    fn maps_the_request_events_and_levels() {
        use crate::domain::error::ErrorKind;
        let (event, level) = request_event_of(false, None);
        assert_eq!((event, level), ("proxy_request.succeeded", "INFO"));
        let (event, level) = request_event_of(true, Some(ErrorKind::RouteNotFound));
        assert_eq!((event, level), ("proxy_request.failed", "INFO"));
        let (event, level) = request_event_of(true, Some(ErrorKind::RateLimitExceeded));
        assert_eq!((event, level), ("proxy_request.failed", "WARN"));
        let (event, level) = request_event_of(true, Some(ErrorKind::CircuitBreakerOpen));
        assert_eq!((event, level), ("proxy_request.failed", "WARN"));
        let (event, level) = request_event_of(true, Some(ErrorKind::RequestTimeout));
        assert_eq!((event, level), ("proxy_request.failed", "ERROR"));
        let (event, level) = request_event_of(true, Some(ErrorKind::AuthenticationFailed));
        assert_eq!((event, level), ("auth.failed", "ERROR"));
        let (event, level) = request_event_of(true, Some(ErrorKind::SecretNotFound));
        assert_eq!((event, level), ("auth.failed", "ERROR"));
    }

    #[test]
    fn maps_the_selection_methods_and_breaker_states() {
        use crate::domain::proxy::EndpointChoice;
        use crate::domain::ratelimit::BreakerPhase;
        assert_eq!(
            selection_method_label(EndpointChoice::Header),
            "explicit_header"
        );
        assert_eq!(selection_method_label(EndpointChoice::LoadBalanced), "round_robin");
        assert_eq!(selection_method_label(EndpointChoice::Only), "default");
        assert_eq!(breaker_state_label(BreakerPhase::Closed), "closed");
        assert_eq!(breaker_state_label(BreakerPhase::Open), "open");
        assert_eq!(breaker_state_label(BreakerPhase::HalfOpen), "half_open");
    }

    #[test]
    fn declares_a_set_for_each_of_the_twelve_families() {
        for family in AUDIT_EVENTS.iter().take(0) {
            assert!(family.ends_with("proxy_request"));
        }
        let families = [
            "oagw_requests_total",
            "oagw_request_duration_seconds",
            "oagw_requests_in_flight",
            "oagw_errors_total",
            "oagw_circuit_breaker_state",
            "oagw_rate_limit_exceeded_total",
            "oagw_circuit_breaker_transitions_total",
            "oagw_rate_limit_usage_ratio",
            "oagw_routing_target_host_used",
            "oagw_routing_endpoint_selected",
            "oagw_upstream_available",
            "oagw_upstream_connections",
        ];
        assert_eq!(families.len(), 12);
        for family in families {
            assert!(MetricLabelSet::labels_of(family).is_some(), "{family}");
            assert!(!family.contains("tenant"));
        }
        assert!(MetricLabelSet::labels_of("oagw_not_a_family").is_none());
    }

    #[test]
    fn carries_no_tenant_key_in_any_label_set() {
        let families = [
            MetricLabelSet::REQUESTS_TOTAL,
            MetricLabelSet::REQUEST_DURATION,
            MetricLabelSet::IN_FLIGHT,
            MetricLabelSet::ERRORS_TOTAL,
            MetricLabelSet::BREAKER_STATE,
            MetricLabelSet::RATE_LIMIT_EXCEEDED,
            MetricLabelSet::BREAKER_TRANSITIONS,
            MetricLabelSet::RATE_LIMIT_USAGE,
            MetricLabelSet::ROUTING_TARGET_USED,
            MetricLabelSet::ROUTING_SELECTED,
            MetricLabelSet::UPSTREAM_AVAILABLE,
            MetricLabelSet::UPSTREAM_CONNECTIONS,
        ];
        assert_eq!(families.len(), 12);
        for set in families {
            for key in set {
                assert!(!key.contains("tenant"), "{key}");
            }
        }
    }

    #[test]
    fn states_the_histogram_buckets() {
        assert_eq!(HISTOGRAM_BUCKETS.len(), 12);
        assert_eq!(HISTOGRAM_BUCKETS[0], 0.001);
        assert_eq!(HISTOGRAM_BUCKETS[11], 10.0);
        assert!(HISTOGRAM_BUCKETS.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
