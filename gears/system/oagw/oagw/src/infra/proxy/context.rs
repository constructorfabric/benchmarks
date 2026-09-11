//! Request and response context entities of the proxy path.
//!
//! The proxy pipeline resolves no store record into a request-scoped state it
//! carries from stage to stage: the alias it selected, the route it matched,
//! the endpoint it dialed and the outcome of the call. The record is the
//! entity entry 2.7 reports the audit line and the metric families from, so it
//! carries routing facts and measurements only — never a request body, never a
//! header value and never credential material.
//!
//! The stages of the record are the states of
//! `cpt-cf-oagw-state-request-lifecycle`, and every stage advance goes through
//! [`RequestContext::advance`], so a stage can never be skipped and the
//! terminal `failed` state is reached only from the states the state machine
//! allows it from.

use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::stream::StreamRecord;
use crate::infra::proxy::call::HttpVersion;

/// The error code a plugin rejection carries in the problem body.
///
/// The canonical rows the chain rejections map onto are shared with the other
/// stages of the pipeline, so the specific code travels beside the row
/// (`cpt-cf-oagw-dod-required-headers`).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// One executed plugin and its outcome, as the request context records it for
/// entry 2.7 (`inst-pc-13`, `inst-alc-13`).
///
/// The record names the plugin identifier and the outcome token only: no
/// configuration value, no credential and no header value is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginOutcome {
    /// The GTS identifier of the executed plugin.
    pub identifier: String,
    /// The plugin type the registry resolved it through.
    pub plugin_type: &'static str,
    /// The phase it executed in.
    pub phase: &'static str,
    /// The outcome the execution ended in (`allow`, `reject`, `error`).
    pub outcome: &'static str,
}

/// The rate-limit observation of one request
/// (`cpt-cf-oagw-state-rate-limit-decision`).
///
/// Every member is a routing fact or a measurement, so the record can travel to
/// entry 2.7 and to the response headers without carrying anything a caller
/// must not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitObservation {
    /// The scope the bucket was keyed on.
    pub scope: String,
    /// Whether the configured scope's identity was unavailable and the request
    /// was accounted under the `tenant` scope key instead (`inst-arl-07`).
    pub scope_fallback: bool,
    /// The disposition of the request (`inst-srl-01` to `inst-srl-06`).
    pub decision: String,
    /// The effective limit of the bucket, in tokens.
    pub limit: u64,
    /// The whole tokens left after the check.
    pub remaining: u64,
    /// The Unix epoch second the bucket returns to full capacity.
    pub reset: Option<u64>,
    /// The whole seconds until the bucket can satisfy the cost.
    pub retry_after: Option<u32>,
    /// Whether the `X-RateLimit-*` headers belong on the response.
    pub response_headers: bool,
}

/// The routing decision that selected an endpoint of a pool.
///
/// Recorded on the request context for the routing metric of entry 2.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// `X-OAGW-Target-Host` named the endpoint.
    ExplicitHeader,
    /// The per-upstream round-robin cursor advanced.
    RoundRobin,
    /// The pool holds exactly one endpoint.
    Default,
}

impl SelectionMethod {
    /// The wire token the routing metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// A stage of the proxy request context.
///
/// The variants are the states of `cpt-cf-oagw-state-request-lifecycle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPhase {
    /// The request reached the proxy endpoint.
    Received,
    /// The alias walk selected an enabled upstream.
    Resolved,
    /// A route matched method and path and its guard rules passed.
    Matched,
    /// The request, body and CORS validation passed and the request-phase hook
    /// points did not reject.
    Validated,
    /// The endpoint was selected, the headers transformed, the breaker admitted
    /// the call and the upstream call was sent.
    Forwarded,
    /// A complete upstream response was received and passed through.
    Responded,
    /// The response was classified as streamed and handed to entry 2.6.
    HandedOff,
    /// A stage or hook point raised a failure the error mapping turned into a
    /// gateway error.
    Failed,
}

impl RequestPhase {
    /// Whether the state machine allows `from` to reach `to`.
    #[must_use]
    pub const fn allows(from: Self, to: Self) -> bool {
        use RequestPhase::{
            Failed, Forwarded, HandedOff, Matched, Received, Responded, Resolved, Validated,
        };
        match (from, to) {
            (Received, Resolved)
            | (Resolved, Matched)
            | (Matched, Validated)
            | (Validated, Forwarded)
            | (Forwarded, Responded)
            | (Forwarded, HandedOff) => true,
            // Any stage that has not produced a client response may fail; a
            // response that already left the pipeline is past the state
            // machine.
            (Received | Resolved | Matched | Validated | Forwarded, Failed) => true,
            _ => false,
        }
    }

    /// The wire token the observability layer labels the stage with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Resolved => "resolved",
            Self::Matched => "matched",
            Self::Validated => "validated",
            Self::Forwarded => "forwarded",
            Self::Responded => "responded",
            Self::HandedOff => "handed_off",
            Self::Failed => "failed",
        }
    }
}

/// Request-scoped state of one proxy request.
///
/// Built by the transport when the request reaches the proxy endpoint and
/// carried through the pipeline; entry 2.7 reads it for the audit line and the
/// metric families. Every field is a routing fact or a measurement.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Correlation identifier carried to the response and to entry 2.7.
    pub trace_id: String,
    /// Alias the request addressed, normalized.
    pub alias: Option<String>,
    /// Request path, as the caller asked for it.
    pub request_path: String,
    /// HTTP method of the request.
    pub method: String,
    /// Whether the request carried an `Origin` header.
    pub cross_origin: bool,
    /// Normalized pattern of the matched route, the label entry 2.7 reports as
    /// `http.route`.
    pub matched_route: Option<String>,
    /// Identifier of the upstream the alias walk selected.
    pub upstream_id: Option<String>,
    /// Host of the endpoint the request was sent to.
    pub endpoint_host: Option<String>,
    /// How the endpoint of the pool was selected.
    pub selection: Option<SelectionMethod>,
    /// HTTP version the per-host cache supplied for the call.
    pub http_version: Option<HttpVersion>,
    /// Request size in bytes, headers excluded.
    pub request_bytes: Option<u64>,
    /// Endpoint hosts of the selected upstream's pool the caller may name with
    /// the routing header, in pool order (ADR 0007 `valid_hosts`).
    pub valid_hosts: Option<Vec<String>>,
    /// GTS `type` identifier of the failure the request ended in.
    pub error_type: Option<&'static str>,
    /// The error code the problem body of a plugin rejection names.
    pub error_code: Option<&'static str>,
    /// The auth method tag the resolved auth plugin injected with
    /// (`inst-pc-13`).
    pub auth_method: Option<String>,
    /// The executed plugin identifiers and their outcomes, in execution order.
    pub plugins: Vec<PluginOutcome>,
    /// The `X-Request-ID` the RequestId transform recorded as the correlation
    /// identifier (`inst-ari-05`).
    pub request_id: Option<String>,
    /// The rate-limit observation of the request, when the chain evaluated one.
    pub rate_limit: Option<RateLimitObservation>,
    /// Whether a rate limit admitted the request while degrading it
    /// (`inst-rl-13`).
    pub degraded: bool,
    /// Current stage of the request.
    pub phase: RequestPhase,
    /// Gateway-added wall-clock duration, in milliseconds.
    pub duration_ms: Option<u64>,
    /// The streamed exchange the entry-2.6 handoff opened, when the request
    /// handed one off: the record entry 2.7 reads the stream facts from. No
    /// header value, no request body byte and no query string is in it.
    pub stream: Option<Arc<StreamRecord>>,
}

impl RequestContext {
    /// A context for a request that reached `path`.
    #[must_use]
    pub fn new(trace_id: String, request_path: String, method: String) -> Self {
        Self {
            trace_id,
            alias: None,
            request_path,
            method,
            cross_origin: false,
            matched_route: None,
            upstream_id: None,
            endpoint_host: None,
            selection: None,
            http_version: None,
            request_bytes: None,
            valid_hosts: None,
            error_type: None,
            error_code: None,
            auth_method: None,
            plugins: Vec::new(),
            request_id: None,
            rate_limit: None,
            degraded: false,
            phase: RequestPhase::Received,
            duration_ms: None,
            stream: None,
        }
    }

    /// Record one executed plugin and its outcome (`inst-alc-13`).
    ///
    /// The record names the plugin and the outcome only, so no credential
    /// material and no configuration value can reach the context entry 2.7
    /// reports.
    pub fn record_plugin(&mut self, outcome: PluginOutcome) {
        self.plugins.push(outcome);
    }

    /// Advance the context to `to`, rejecting a transition the state machine
    /// does not allow.
    ///
    /// # Errors
    ///
    /// Returns the mapped `502` of a stage sequence that skips a stage, which
    /// is a programming fault of the pipeline rather than a caller fault.
    pub fn advance(&mut self, to: RequestPhase) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-01
        // The state machine guards every stage advance, so a stage cannot be
        // skipped and the reported state is always the one the pipeline is in.
        if !RequestPhase::allows(self.phase, to) {
            return Err(DomainError::ProtocolError {
                detail: format!(
                    "the request context cannot move from {} to {}",
                    self.phase.as_str(),
                    to.as_str()
                ),
            });
        }
        self.phase = to;
        Ok(())
        // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-01
    }

    /// Move the context to `failed`, recording the GTS type of the failure.
    ///
    /// Every state the pipeline can be in before a response left it may reach
    /// `failed`; a context that already produced a response is past the state
    /// machine and keeps its terminal state.
    pub fn fail(&mut self, error: &DomainError) {
        // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-07
        // `received`, `resolved`, `matched`, `validated` and `forwarded` all
        // reach `failed` when the error mapping turns a stage outcome into a
        // gateway error.
        if RequestPhase::allows(self.phase, RequestPhase::Failed) {
            self.phase = RequestPhase::Failed;
            self.error_type = Some(error.gts_id());
        }
        // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-07
    }
}

/// Response-scoped state of one proxy request.
///
/// Built when the upstream response head is classified, before its body is
/// read, so the classification is the decision the body handling follows.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Status the upstream returned.
    pub status: u16,
    /// Whether the response head declares a streamed body or an upgrade.
    pub streamed: bool,
    /// Whether the open exchange was handed to entry 2.6.
    pub handed_off: bool,
    /// HTTP version the upstream negotiated.
    pub http_version: HttpVersion,
    /// Error source the response is classified with.
    pub error_source: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> RequestContext {
        RequestContext::new(
            "trace-1".to_owned(),
            "/oagw/v1/proxy/api.vendor.com".to_owned(),
            "GET".to_owned(),
        )
    }

    #[test]
    fn a_context_starts_in_the_received_state() {
        let context = context();
        assert_eq!(context.phase, RequestPhase::Received);
        assert_eq!(RequestPhase::Received.as_str(), "received");
        assert!(context.alias.is_none());
        assert!(context.matched_route.is_none());
    }

    #[test]
    fn the_happy_path_walks_every_state_in_order() {
        let mut context = context();
        for stage in [
            RequestPhase::Resolved,
            RequestPhase::Matched,
            RequestPhase::Validated,
            RequestPhase::Forwarded,
            RequestPhase::Responded,
        ] {
            context.advance(stage).expect("ordered advance");
            assert_eq!(context.phase, stage);
        }
    }

    #[test]
    fn a_streamed_response_reaches_the_handed_off_state() {
        let mut context = context();
        for stage in [
            RequestPhase::Resolved,
            RequestPhase::Matched,
            RequestPhase::Validated,
            RequestPhase::Forwarded,
        ] {
            context.advance(stage).expect("ordered advance");
        }
        context
            .advance(RequestPhase::HandedOff)
            .expect("the forwarded state hands off");
        assert_eq!(RequestPhase::HandedOff.as_str(), "handed_off");
    }

    #[test]
    fn a_skipped_stage_is_rejected() {
        let mut context = context();
        let error = context
            .advance(RequestPhase::Forwarded)
            .expect_err("a stage may not be skipped");
        assert_eq!(error.status(), 502, "{error}");
        assert_eq!(context.phase, RequestPhase::Received, "the state is kept");
    }

    #[test]
    fn every_stage_before_a_response_may_fail() {
        for start in [
            RequestPhase::Received,
            RequestPhase::Resolved,
            RequestPhase::Matched,
            RequestPhase::Validated,
            RequestPhase::Forwarded,
        ] {
            assert!(
                RequestPhase::allows(start, RequestPhase::Failed),
                "{} must reach failed",
                start.as_str()
            );
        }
        assert!(!RequestPhase::allows(RequestPhase::Responded, RequestPhase::Failed));
        assert!(!RequestPhase::allows(RequestPhase::HandedOff, RequestPhase::Failed));
    }

    #[test]
    fn failing_records_the_error_type_and_the_state() {
        let mut context = context();
        context.advance(RequestPhase::Resolved).expect("advance");
        let error = DomainError::RouteNotFound {
            detail: "no route".to_owned(),
        };
        context.fail(&error);
        assert_eq!(context.phase, RequestPhase::Failed);
        assert_eq!(context.error_type, Some(error.gts_id()));
    }

    #[test]
    fn a_terminal_response_state_is_never_overwritten_by_a_failure() {
        let mut context = context();
        for stage in [
            RequestPhase::Resolved,
            RequestPhase::Matched,
            RequestPhase::Validated,
            RequestPhase::Forwarded,
            RequestPhase::Responded,
        ] {
            context.advance(stage).expect("ordered advance");
        }
        context.fail(&DomainError::DownstreamError {
            detail: "late".to_owned(),
        });
        assert_eq!(context.phase, RequestPhase::Responded);
        assert!(context.error_type.is_none());
    }

    #[test]
    fn selection_methods_carry_the_wire_tokens_of_the_routing_metric() {
        assert_eq!(SelectionMethod::ExplicitHeader.as_str(), "explicit_header");
        assert_eq!(SelectionMethod::RoundRobin.as_str(), "round_robin");
        assert_eq!(SelectionMethod::Default.as_str(), "default");
    }

    #[test]
    fn an_executed_plugin_is_recorded_with_its_outcome_only() {
        let mut context = context();
        context.record_plugin(PluginOutcome {
            identifier: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned(),
            plugin_type: "auth",
            phase: "on_request",
            outcome: "allow",
        });
        context.record_plugin(PluginOutcome {
            identifier: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
                .to_owned(),
            plugin_type: "guard",
            phase: "on_request",
            outcome: "reject",
        });
        assert_eq!(context.plugins.len(), 2);
        assert_eq!(context.plugins[0].outcome, "allow");
        let rendered = format!("{context:?}").to_lowercase();
        assert!(!rendered.contains("bearer"), "{rendered}");
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn the_plugin_records_start_empty() {
        let context = context();
        assert!(context.plugins.is_empty());
        assert!(context.auth_method.is_none());
        assert!(context.request_id.is_none());
        assert!(context.rate_limit.is_none());
        assert!(!context.degraded);
        assert!(context.error_code.is_none());
    }
}
