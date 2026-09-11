//! The ADR 0001 audit line of the OAGW gear (entry 2.7).
//!
//! One closed request produces exactly one line, built by
//! `cpt-cf-oagw-algo-audit-line` from the fields the closed request context
//! carries and from nothing else: the builder's input is the allowlist, so no
//! request body byte, no response body byte, no query string, no header value
//! other than the correlation identifier and no credential value can reach a
//! field by construction.
//!
//! The line is serialized in the declaration order of [`AuditRecord`], which is
//! the ADR 0001 key order, and emitted as a single JSON document.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use toolkit_gts::gts_id;

use crate::domain::error::DomainError;
use crate::domain::stream::{CloseReason, StreamOutcome};
use crate::infra::proxy::context::RequestContext;

/// The `event` value of a proxied request's audit line.
pub const AUDIT_EVENT: &str = "proxy_request";

/// The fourteen keys of ADR 0001, in the order the line carries them.
pub const AUDIT_KEYS: [&str; 14] = [
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

/// The only key the line may add beyond the fourteen of ADR 0001.
pub const KEY_ERROR_MESSAGE: &str = "error_message";

/// The GTS `type` identifier a stream close that aborted folds into
/// `error_type`.
pub const STREAM_ABORTED_TYPE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");

/// The GTS `type` identifier a stream close that timed out folds into
/// `error_type`.
pub const STREAM_IDLE_TIMEOUT_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");

/// The severity the audit line is recorded at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditLevel {
    /// The request was served.
    Info,
    /// The gateway refused a request whose input caused the refusal, or shed it.
    Warn,
    /// An upstream failure, a timeout or a credential failure.
    Error,
}

impl AuditLevel {
    /// The name the level is recorded with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// The level a gateway failure is recorded at (`inst-ob-aline-06`).
    ///
    /// A rate-limit rejection, an open circuit breaker and the gateway-rejected
    /// client-input class are warnings: the gateway refused the request rather
    /// than failing to serve it. An upstream failure, a timeout and a
    /// credential failure are errors: the gateway could not do what the caller
    /// asked of it. Every other outcome is informational. A `409` PluginInUse
    /// is the management-plane class of that vocabulary and is a warning, as
    /// the acceptance criteria of the feature fix it.
    #[must_use]
    pub fn for_failure(error: &DomainError) -> Self {
        match error {
            DomainError::RateLimitExceeded { .. }
            | DomainError::CircuitBreakerOpen { .. }
            | DomainError::RouteError { .. }
            | DomainError::ValidationError { .. }
            | DomainError::MissingTargetHost { .. }
            | DomainError::InvalidTargetHost { .. }
            | DomainError::UnknownTargetHost { .. }
            | DomainError::RouteNotFound { .. }
            | DomainError::PayloadTooLarge { .. } => Self::Warn,
            DomainError::AuthenticationFailed { .. }
            | DomainError::SecretNotFound { .. }
            | DomainError::ProtocolError { .. }
            | DomainError::DownstreamError { .. }
            | DomainError::StreamAborted { .. }
            | DomainError::LinkUnavailable { .. }
            | DomainError::PluginNotFound { .. }
            | DomainError::ConnectionTimeout { .. }
            | DomainError::RequestTimeout { .. }
            | DomainError::IdleTimeout { .. } => Self::Error,
            DomainError::CorsOriginNotAllowed { .. }
            | DomainError::CorsMethodNotAllowed { .. } => Self::Warn,
            DomainError::PluginInUse { .. } => Self::Warn,
        }
    }

    /// The level a streamed close is recorded at (`inst-ob-aline-06`).
    ///
    /// A streamed exchange carries no [`DomainError`], so its level comes from
    /// the close reason the lifecycle recorded: an abort and an idle timeout
    /// are the `502` and `504` classes of the error table and are `ERROR`,
    /// while a close either side ended normally is `INFO`.
    #[must_use]
    pub const fn for_close(reason: CloseReason) -> Self {
        match reason {
            CloseReason::Aborted | CloseReason::IdleTimeout => Self::Error,
            CloseReason::UpstreamClosed
            | CloseReason::ClientDisconnected
            | CloseReason::Rejected => Self::Info,
        }
    }
}

impl Serialize for AuditLevel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AuditLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let level = String::deserialize(deserializer)?;
        match level.as_str() {
            "WARN" => Ok(Self::Warn),
            "ERROR" => Ok(Self::Error),
            "INFO" => Ok(Self::Info),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["INFO", "WARN", "ERROR"],
            )),
        }
    }
}

/// The values the response sizes of a streamed exchange are folded from
/// (`inst-ob-aline-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFold {
    /// Why the exchange ended, as the recorded close reason.
    pub reason: &'static str,
    /// The side that closed the exchange.
    pub closing_side: &'static str,
    /// Bytes the upstream sent and the gateway relayed to the client.
    pub bytes_downstream: u64,
    /// Bytes the client sent and the gateway relayed to the upstream.
    pub bytes_upstream: u64,
    /// Whether the close was an abort or a timeout, which folds the close
    /// reason into `error_type`.
    pub failed: bool,
    /// The GTS `type` identifier the stream lifecycle recorded for the close.
    pub error_type: Option<&'static str>,
    /// The close reason the fold was taken from.
    pub close: CloseReason,
}

impl StreamFold {
    /// The fold of one recorded stream outcome.
    #[must_use]
    pub fn of(outcome: &StreamOutcome) -> Self {
        Self {
            reason: outcome.reason.as_str(),
            closing_side: outcome.closing_side,
            bytes_downstream: outcome.bytes_downstream,
            bytes_upstream: outcome.bytes_upstream,
            failed: failed_close(outcome.reason),
            error_type: outcome.error_type,
            close: outcome.reason,
        }
    }
}

/// Whether a close reason is an abort or a timeout.
#[must_use]
pub const fn failed_close(reason: CloseReason) -> bool {
    matches!(reason, CloseReason::Aborted | CloseReason::IdleTimeout)
}

/// The GTS `type` identifier a close reason folds into `error_type`.
#[must_use]
pub const fn close_error_type(reason: CloseReason) -> &'static str {
    match reason {
        CloseReason::IdleTimeout => STREAM_IDLE_TIMEOUT_TYPE,
        _ => STREAM_ABORTED_TYPE,
    }
}

/// The audit line of one closed request.
///
/// The declaration order is the ADR 0001 key order and the serialization emits
/// the fields in that order; `error_message` is the only key added beyond the
/// fourteen, and only on a failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// The emission instant, UTC with millisecond precision.
    pub timestamp: String,
    /// The severity the line is recorded at.
    pub level: AuditLevel,
    /// The literal `proxy_request`.
    pub event: String,
    /// The correlation identifier, the only header-derived value on the line.
    pub request_id: Option<String>,
    /// The tenant the request was authenticated for.
    pub tenant_id: Option<String>,
    /// The principal the request was authenticated as.
    pub principal_id: Option<String>,
    /// The resolved upstream alias, `null` when none was resolved.
    pub host: Option<String>,
    /// The proxied path without its query string.
    pub path: Option<String>,
    /// The request method as received.
    pub method: Option<String>,
    /// The numeric status the client received.
    pub status: Option<u16>,
    /// The request's own start-to-close interval, in milliseconds.
    pub duration_ms: Option<u64>,
    /// Bytes the client sent, or the upstream direction of a streamed exchange.
    pub request_size: Option<u64>,
    /// Bytes the client received.
    pub response_size: Option<u64>,
    /// The GTS `type` identifier of the failure, `null` on a success.
    pub error_type: Option<String>,
    /// The redacted detail the client already received, on a failure only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// The fields of a closed request the audit line is built from.
///
/// This is the whole input of `cpt-cf-oagw-algo-audit-line`: a field that is
/// not here cannot be emitted, which is the redaction rule applied by
/// construction rather than by filtering.
#[derive(Debug, Clone, Copy)]
pub struct AuditInput<'a> {
    /// The closed request context.
    pub context: &'a RequestContext,
    /// The numeric status the client received.
    pub status: u16,
    /// The request's own start-to-close interval.
    pub duration: Duration,
    /// Bytes the client received, when the exchange produced a counted body.
    pub response_bytes: Option<u64>,
    /// The GTS `type` identifier of the failure, when the outcome is one.
    pub error: Option<&'a DomainError>,
    /// The tenant the request was authenticated for, when the pipeline knows it.
    pub tenant_id: Option<&'a str>,
    /// The principal the request was authenticated as, when the pipeline knows
    /// it.
    pub principal_id: Option<&'a str>,
    /// The fold of the stream outcome, when the response was streamed.
    pub stream: Option<StreamFold>,
}

impl AuditRecord {
    /// Build the audit line of one closed request
    /// (`cpt-cf-oagw-algo-audit-line`).
    #[must_use]
    pub fn build(input: &AuditInput<'_>) -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-01
        // `timestamp` is the emission instant in UTC with millisecond precision,
        // `event` is the literal `proxy_request` and `status` is the numeric
        // status the client received.
        let context = input.context;
        let event = AUDIT_EVENT.to_owned();
        let timestamp = timestamp_now();
        let status = Some(input.status);
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-01

        // @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-03
        // @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-01
        // The correlation identifier is read from the request context: the
        // identifier the entry-2.5 RequestId transform recorded on the context
        // when it did, else the identifier the pipeline generated at
        // `inst-pe-req-03`. No header value is read to obtain it.
        let request_id = correlation_id(context);
        // @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-01
        // @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-03

        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-02
        // `host` is the resolved upstream alias, the same value the `host`
        // metric label carries. When no upstream was resolved it is JSON
        // `null` and never the requested alias, path or authority.
        let host = context.alias.clone();
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-02

        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-02a
        // `path` is the proxied path with its query string removed, `method`
        // the request method as received, and the two size fields the byte
        // counts the exchange records.
        let path = Some(path_of(context));
        let method = Some(context.method.clone());
        let (request_size, response_size) = byte_counts(context, input);
        let duration_ms = Some(u64::try_from(input.duration.as_millis()).unwrap_or_default());
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-02a

        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-03
        // The tenant and the principal are the identifiers the pipeline
        // authenticated the request with, never a header value; a field the
        // emission cannot know is `null` and is never omitted.
        let tenant_id = non_empty(input.tenant_id).map(str::to_owned);
        let principal_id = non_empty(input.principal_id).map(str::to_owned);
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-03

        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-04
        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-05
        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-05
        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-05a
        // On a failure the line carries the GTS `type` identifier of the
        // canonical error the entry-2.4 mapping produced and its redacted
        // `detail`, which is the value the client already received in the
        // problem body.
        let (error_type, error_message) = match input.error {
            Some(error) => (
                Some(error.gts_id().to_owned()),
                Some(redact(error.detail())),
            ),
            // A streamed exchange carries no canonical error: its failure is the
            // close reason the lifecycle recorded, whose identifier is the
            // `error_type` the fold carries and whose message is the fixed
            // rendering of that reason, which holds no request-derived value.
            None => (None, close_message(input.stream)),
        };
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-05a
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-05
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-05
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-04

        let level = if let Some(error) = input.error {
            // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-06
            // A rate-limit rejection, an open breaker and a gateway-rejected
            // client-input class are `WARN`; an upstream failure, a timeout and
            // a credential failure are `ERROR`.
            AuditLevel::for_failure(error)
            // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-06
        } else if let Some(fold) = input.stream {
            // A streamed close is levelled from the close reason: an abort and
            // an idle timeout are `ERROR`, a normal close is `INFO`.
            AuditLevel::for_close(fold.close)
        } else {
            // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-06
            // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-07
            // On a success `error_type` is JSON `null`, no `error_message` is
            // added and the level is `INFO`.
            AuditLevel::Info
            // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-07
            // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-06
        };

        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-08
        // The redaction rule is the key set itself: the record carries the
        // fourteen keys of ADR 0001 and nothing else, so no request body, no
        // response body, no query string and no header value other than the
        // correlation identifier can reach a field, and the only free-text
        // field is the detail the redactor has already stripped.
        // @cpt-begin:cpt-cf-oagw-dod-audit-fields:p1:inst-full
        // The level vocabulary of DESIGN §4.3 is applied above, and no API key,
        // token, credential or resolved `cred://` value is logged at any level,
        // in any field, on any code path: the fields that could carry one do
        // not exist on this record.
        Self {
            timestamp,
            level,
            event,
            // @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-04a
            // The identifier is placed on the record as `request_id`; on a
            // gateway failure it is the value the problem body's `trace_id`
            // extension field carries, which the error mapping copies from the
            // same context.
            request_id,
            // @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-04a
            tenant_id,
            principal_id,
            host,
            path,
            method,
            status,
            duration_ms,
            request_size,
            response_size: response_size.or(input.response_bytes),
            error_type: error_type.or_else(|| stream_error_type(input.stream)),
            error_message,
        }
        // @cpt-end:cpt-cf-oagw-dod-audit-fields:p1:inst-full
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-08
    }

    /// The key set the line carries, in serialization order.
    #[must_use]
    pub fn keys(&self) -> Vec<&'static str> {
        let mut keys = AUDIT_KEYS.to_vec();
        if self.error_message.is_some() {
            keys.push(KEY_ERROR_MESSAGE);
        }
        keys
    }

    // @cpt-begin:cpt-cf-oagw-dod-audit-line:p1:inst-full
    /// Serialize the line as one JSON document in the ADR 0001 key order.
    ///
    /// Exactly one document is produced per closed request, carrying the
    /// fourteen keys in order with `event` set to the literal `proxy_request`,
    /// `error_type` JSON `null` on a success and the GTS `type` identifier plus
    /// an added `error_message` on a failure.
    #[must_use]
    pub fn to_line(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
    // @cpt-end:cpt-cf-oagw-dod-audit-line:p1:inst-full
}

/// The correlation identifier of a request context
/// (`cpt-cf-oagw-algo-correlation-propagate`).
///
/// The identifier the entry-2.5 RequestId transform recorded on the context
/// wins, because the outbound request and the response carry that one; the
/// identifier the pipeline opened at `inst-pe-req-03` is the fallback and is
/// present on every request, including one rejected before authentication. An
/// identifier that arrives empty is replaced by the context's own identifier
/// and is never recorded as an empty string.
#[must_use]
pub fn correlation_id(context: &RequestContext) -> Option<String> {
    super::correlation::correlation_id(context)
}

/// The proxied path of a request context, without its query string.
///
/// The path the pipeline matched on is the path the line records; a query
/// string is never carried, so the separator that would introduce one is
/// stripped with everything after it.
#[must_use]
pub fn path_of(context: &RequestContext) -> String {
    context
        .request_path
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The two byte counts a closed request records.
///
/// A streamed exchange carries the counts entry 2.6 recorded per direction, so
/// the direction the client sent is `request_size` and the direction the client
/// received is `response_size`; the line is emitted once, at the close, never
/// per chunk. An exchange that was not streamed carries the byte count the
/// context recorded for its body, and no response byte count when the gateway
/// relayed a body it did not count.
#[must_use]
pub fn byte_counts(
    context: &RequestContext,
    input: &AuditInput<'_>,
) -> (Option<u64>, Option<u64>) {
    if let Some(fold) = input.stream {
        return (Some(fold.bytes_upstream), Some(fold.bytes_downstream));
    }
    if let Some(outcome) = context.stream.as_ref().and_then(|record| record.outcome()) {
        return (Some(outcome.bytes_upstream), Some(outcome.bytes_downstream));
    }
    (context.request_bytes, None)
}

/// The GTS `type` identifier a streamed close folds into `error_type`.
fn stream_error_type(stream: Option<StreamFold>) -> Option<String> {
    let fold = stream?;
    if !fold.failed {
        return None;
    }
    Some(
        fold.error_type
            .map_or_else(|| close_error_type(fold.close).to_owned(), str::to_owned),
    )
}

/// The message a streamed close is recorded with.
///
/// A streamed exchange has no canonical error to take a detail from, so a
/// failed close is recorded with the fixed rendering of its close reason: a
/// constant that holds no request-derived value and needs no redaction. A
/// close that is not a failure records no message at all, as a success line
/// never carries one.
fn close_message(stream: Option<StreamFold>) -> Option<String> {
    let fold = stream?;
    if !fold.failed {
        return None;
    }
    Some(match fold.close {
        CloseReason::IdleTimeout => "the streamed exchange exceeded its idle window".to_owned(),
        _ => "the streamed exchange was aborted".to_owned(),
    })
}

/// The `Option<&str>` a context field is recorded with: empty is unknown.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// The redaction a detail receives before it is recorded
/// (`inst-ob-aline-08`).
///
/// A `DomainError` detail is the text the client received in the problem body,
/// and several of its rows interpolate the request-derived value that caused
/// the refusal — a disallowed `Origin`, a method an origin may not use, a
/// rate-limit bucket key that carries a peer address or a subject, an alias the
/// request spelled. Those are header values, a peer address and personal data
/// the redaction rule bans from every emitted line, so the interpolation
/// convention the details follow — the variable part inside a quoted span — is
/// what this function removes: the content of each backtick- and
/// single-quote-delimited span is replaced with a placeholder and only the
/// fixed message around the spans is recorded.
///
/// A span cannot smuggle a value past this: its whole content is replaced, and
/// a quote character inside a value only ends the span early. A span that never
/// closes ends the detail there, because what follows an unclosed quote is
/// unbounded input.
#[must_use]
pub fn redact(detail: &str) -> String {
    const PLACEHOLDER: &str = "<redacted>";
    const QUOTES: [char; 2] = ['`', '\''];

    let mut redacted = String::with_capacity(detail.len());
    let mut rest = detail;
    'outer: while !rest.is_empty() {
        let Some(open) = rest.find(QUOTES) else {
            redacted.push_str(rest);
            break;
        };
        redacted.push_str(&rest[..open]);
        let quote = rest[open..].chars().next().unwrap_or('`');
        let body = &rest[open + quote.len_utf8()..];
        match body.find(quote) {
            Some(close) => {
                redacted.push_str(PLACEHOLDER);
                rest = &body[close + quote.len_utf8()..];
            }
            None => break 'outer,
        }
    }
    redacted
}

/// The emission instant, UTC with millisecond precision.
#[must_use]
pub fn timestamp_now() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_utc_millis(since_epoch)
}

/// The ISO-8601 UTC instant of a duration since the Unix epoch.
#[must_use]
pub fn format_utc_millis(since_epoch: Duration) -> String {
    let seconds = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let hour = (seconds / 3_600) % 24;
    let minute = (seconds / 60) % 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// The civil date of a count of days since the Unix epoch.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

// @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-08
/// The rule violations a serialized line carries, if any
/// (`inst-ob-aline-08`).
///
/// The redaction rule is applied by construction — the builder reads no body,
/// no query string and no header value — and this check is what the tests hold
/// it to: the line carries no key outside the recorded set, no query string in
/// the path and no credential material in any value. The correlation identifier
/// is the only header-derived value any line of this feature carries, so a
/// second one would show up here.
#[must_use]
pub fn redaction_findings(line: &str) -> Vec<String> {
    let mut findings = Vec::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        findings.push("the line is not a single JSON document".to_owned());
        return findings;
    };
    let Some(object) = value.as_object() else {
        findings.push("the line is not a JSON object".to_owned());
        return findings;
    };
    for key in object.keys() {
        let recorded = AUDIT_KEYS.contains(&key.as_str()) || key == KEY_ERROR_MESSAGE;
        if !recorded {
            findings.push(format!("a key outside the recorded set: {key}"));
        }
    }
    if object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|path| path.contains(['?', '#']))
    {
        findings.push("a query string in the path".to_owned());
    }
    findings
}
// @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-08

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn context() -> RequestContext {
        RequestContext::new(
            "trace-1".to_owned(),
            "/oagw/v1/proxy/api.vendor.com/v1/things?q=1".to_owned(),
            "GET".to_owned(),
        )
    }

    fn input(context: &RequestContext) -> AuditInput<'_> {
        AuditInput {
            context,
            status: 200,
            duration: Duration::from_millis(12),
            response_bytes: None,
            error: None,
            tenant_id: Some("11111111-1111-1111-1111-111111111111"),
            principal_id: Some("22222222-2222-2222-2222-222222222222"),
            stream: None,
        }
    }

    #[test]
    fn a_detail_that_interpolates_request_derived_values_records_no_value() {
        // The rows whose detail carries the value that caused the refusal: a
        // disallowed `Origin`, a method the origin may not use, a rate-limit
        // bucket key that carries a peer address or a subject, and an alias the
        // request spelled. The message around the interpolation is kept, the
        // interpolated value is not.
        let origin = DomainError::CorsOriginNotAllowed {
            detail: "the origin `https://evil.example.com` is not allowed".to_owned(),
        };
        assert_eq!(
            redact(origin.detail()),
            "the origin <redacted> is not allowed",
            "{origin}"
        );

        let method = DomainError::CorsMethodNotAllowed {
            detail: "the method `TRACE` is not allowed for the origin".to_owned(),
        };
        assert_eq!(
            redact(method.detail()),
            "the method <redacted> is not allowed for the origin",
            "{method}"
        );

        let bucket = DomainError::RateLimitExceeded {
            detail: "the bucket `rate_limit:3303c846-0feb-43b8-ac9e-ca739414648b:ip:203.0.113.7` \
                     cannot satisfy the request cost"
                .to_owned(),
            retry_after_seconds: None,
        };
        let line = redact(bucket.detail());
        assert_eq!(
            line,
            "the bucket <redacted> cannot satisfy the request cost",
            "{bucket}"
        );
        assert!(
            !line.contains("203.0.113.7"),
            "a peer address is not recorded: {line}"
        );

        let alias = DomainError::RouteNotFound {
            detail: "no tenant of the chain holds the alias `absent.vendor.com`".to_owned(),
        };
        assert_eq!(
            redact(alias.detail()),
            "no tenant of the chain holds the alias <redacted>",
            "{alias}"
        );
    }

    #[test]
    fn a_value_that_carries_the_quote_character_cannot_smuggle_text_past_the_redaction() {
        // A header value is arbitrary text, so a value that itself contains the
        // interpolation quote only ends its own span early: what it carries
        // stays inside the placeholder's span and is dropped with it.
        let forged = "the origin `https://good.example.com` is not allowed";
        assert_eq!(redact(forged), "the origin <redacted> is not allowed");
        assert_eq!(redact("no quoted value at all"), "no quoted value at all");
        // An unclosed quote ends the detail: what follows it is unbounded.
        assert_eq!(redact("the origin `https://evil.example.com"), "the origin ");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn the_line_carries_the_fourteen_keys_of_adr_0001_in_order() {
        let context = context();
        let record = AuditRecord::build(&input(&context));
        assert_eq!(record.keys(), AUDIT_KEYS.to_vec());
        let line = record.to_line().expect("the line serializes");
        let mut previous = 0;
        for key in AUDIT_KEYS {
            let at =
                line.find(&format!("\"{key}\"")).unwrap_or_else(|| panic!("{key} is on {line}"));
            assert!(at >= previous, "{key} follows the preceding key");
            previous = at;
        }
    }

    #[test]
    fn a_success_records_a_null_error_type_and_no_error_message() {
        let context = context();
        let record = AuditRecord::build(&input(&context));
        assert_eq!(record.error_type, None);
        assert_eq!(record.error_message, None);
        assert_eq!(record.level, AuditLevel::Info);
        assert_eq!(record.level.as_str(), "INFO");
        assert_eq!(record.event, "proxy_request");
        let line = record.to_line().expect("the line serializes");
        assert!(!line.contains(KEY_ERROR_MESSAGE));
        assert!(line.contains("\"error_type\":null"));
    }

    #[test]
    fn a_failure_adds_the_gts_type_and_the_redacted_detail() {
        let context = context();
        let error = DomainError::DownstreamError {
            detail: "upstream answered 500".to_owned(),
        };
        let mut observation = input(&context);
        observation.error = Some(&error);
        let record = AuditRecord::build(&observation);
        assert_eq!(record.error_type.as_deref(), Some(error.gts_id()));
        assert_eq!(record.error_message.as_deref(), Some(error.detail()));
        assert_eq!(record.keys().last(), Some(&KEY_ERROR_MESSAGE));
        assert_eq!(record.level, AuditLevel::Error);
    }

    #[test]
    fn the_level_vocabulary_follows_the_failure_class() {
        let warn = [
            DomainError::RateLimitExceeded {
                detail: "d".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::CircuitBreakerOpen {
                detail: "d".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::ValidationError { detail: "d".to_owned() },
            DomainError::RouteError { detail: "d".to_owned() },
            DomainError::MissingTargetHost { detail: "d".to_owned() },
            DomainError::InvalidTargetHost { detail: "d".to_owned() },
            DomainError::UnknownTargetHost { detail: "d".to_owned() },
            DomainError::RouteNotFound { detail: "d".to_owned() },
            DomainError::PayloadTooLarge { detail: "d".to_owned() },
        ];
        for error in warn {
            assert_eq!(AuditLevel::for_failure(&error), AuditLevel::Warn, "{error}");
        }
        let failure = [
            DomainError::AuthenticationFailed { detail: "d".to_owned() },
            DomainError::SecretNotFound { detail: "d".to_owned() },
            DomainError::ProtocolError { detail: "d".to_owned() },
            DomainError::DownstreamError { detail: "d".to_owned() },
            DomainError::StreamAborted { detail: "d".to_owned() },
            DomainError::LinkUnavailable {
                detail: "d".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::PluginNotFound { detail: "d".to_owned() },
            DomainError::ConnectionTimeout {
                detail: "d".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::RequestTimeout {
                detail: "d".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::IdleTimeout {
                detail: "d".to_owned(),
                retry_after_seconds: None,
            },
        ];
        for error in failure {
            assert_eq!(AuditLevel::for_failure(&error), AuditLevel::Error, "{error}");
        }
        let info = [
            DomainError::CorsOriginNotAllowed { detail: "d".to_owned() },
            DomainError::CorsMethodNotAllowed { detail: "d".to_owned() },
            DomainError::PluginInUse { detail: "d".to_owned() },
        ];
        for error in info {
            assert_eq!(AuditLevel::for_failure(&error), AuditLevel::Warn, "{error}");
        }
    }

    #[test]
    fn the_host_is_the_resolved_alias_and_null_without_one() {
        let mut context = context();
        context.alias = Some("api.vendor.com".to_owned());
        let record = AuditRecord::build(&input(&context));
        assert_eq!(record.host.as_deref(), Some("api.vendor.com"));

        let mut unresolved = RequestContext::new(
            "trace-3".to_owned(),
            "/oagw/v1/proxy/absent.vendor.com/v1".to_owned(),
            "GET".to_owned(),
        );
        unresolved.alias = None;
        let record = AuditRecord::build(&input(&unresolved));
        assert_eq!(record.host, None, "a requested alias is not a host value");
        let line = record.to_line().expect("the line serializes");
        let document = serde_json::from_str::<serde_json::Value>(&line).expect("one JSON document");
        assert!(document["host"].is_null(), "the key is present as null: {line}");
        assert!(redaction_findings(&line).is_empty());
    }

    #[test]
    fn the_path_is_recorded_without_its_query_string() {
        let context = context();
        let record = AuditRecord::build(&input(&context));
        assert_eq!(
            record.path.as_deref(),
            Some("/oagw/v1/proxy/api.vendor.com/v1/things")
        );
    }

    #[test]
    fn an_unknown_context_field_is_null_and_never_omitted() {
        // A request the pipeline opened with no identifier of its own and whose
        // exchange recorded no tenant, principal or resolved upstream.
        let context = RequestContext::new(
            String::new(),
            "/oagw/v1/proxy/api.vendor.com/v1".to_owned(),
            "POST".to_owned(),
        );
        let mut observation = input(&context);
        observation.tenant_id = None;
        observation.principal_id = None;
        let record = AuditRecord::build(&observation);
        assert_eq!(record.request_id, None);
        assert_eq!(record.tenant_id, None);
        assert_eq!(record.principal_id, None);
        assert_eq!(record.host, None);
        let line = record.to_line().expect("the line serializes");
        for key in ["tenant_id", "principal_id", "host", "request_id"] {
            assert!(
                line.contains(&format!("\"{key}\":null")),
                "{key} is present as null: {line}"
            );
        }
        assert_eq!(AUDIT_KEYS.len(), 14);
    }

    #[test]
    fn the_sizes_are_the_byte_counts_the_exchange_records() {
        let mut context = context();
        context.request_bytes = Some(12);
        let record = AuditRecord::build(&input(&context));
        assert_eq!(record.request_size, Some(12));
        assert_eq!(record.response_size, None);

        let mut observation = input(&context);
        observation.response_bytes = Some(48);
        let record = AuditRecord::build(&observation);
        assert_eq!(record.request_size, Some(12));
        assert_eq!(record.response_size, Some(48));
    }

    #[test]
    fn a_streamed_exchange_folds_the_direction_of_each_byte_count() {
        let context = context();
        let outcome = StreamOutcome::terminal(
            CloseReason::UpstreamClosed,
            crate::domain::stream::SIDE_UPSTREAM,
            4096,
            128,
            None,
            Some(30),
        );
        let mut observation = input(&context);
        observation.stream = Some(StreamFold::of(&outcome));
        let record = AuditRecord::build(&observation);
        assert_eq!(record.request_size, Some(128));
        assert_eq!(record.response_size, Some(4096));
        assert_eq!(record.error_type, None);
        assert_eq!(record.level, AuditLevel::Info);
    }

    #[test]
    fn an_aborted_stream_folds_the_close_reason_into_error_type() {
        let context = context();
        let outcome = StreamOutcome::terminal(
            CloseReason::Aborted,
            crate::domain::stream::SIDE_CLIENT,
            64,
            16,
            None,
            Some(30),
        );
        let mut observation = input(&context);
        observation.stream = Some(StreamFold::of(&outcome));
        let record = AuditRecord::build(&observation);
        assert_eq!(record.error_type.as_deref(), Some(STREAM_ABORTED_TYPE));
        // An abort is the `502` class of the error table, so the line is
        // recorded at `ERROR` with the message the close reason renders.
        assert_eq!(record.level, AuditLevel::Error);
        assert_eq!(
            record.error_message.as_deref(),
            Some("the streamed exchange was aborted")
        );

        let outcome = StreamOutcome::terminal(
            CloseReason::IdleTimeout,
            crate::domain::stream::SIDE_NONE,
            0,
            0,
            None,
            Some(30),
        );
        observation.stream = Some(StreamFold::of(&outcome));
        let record = AuditRecord::build(&observation);
        assert_eq!(record.error_type.as_deref(), Some(STREAM_IDLE_TIMEOUT_TYPE));
        assert_eq!(record.level, AuditLevel::Error);
        assert_eq!(
            record.error_message.as_deref(),
            Some("the streamed exchange exceeded its idle window")
        );
    }

    #[test]
    fn a_normal_stream_close_is_info_without_a_message() {
        for reason in [
            CloseReason::UpstreamClosed,
            CloseReason::ClientDisconnected,
            CloseReason::Rejected,
        ] {
            let context = context();
            let outcome = StreamOutcome::terminal(
                reason,
                crate::domain::stream::SIDE_UPSTREAM,
                64,
                16,
                None,
                Some(30),
            );
            let mut observation = input(&context);
            observation.stream = Some(StreamFold::of(&outcome));
            let record = AuditRecord::build(&observation);
            assert_eq!(record.level, AuditLevel::for_close(reason), "{}", reason.as_str());
            assert_eq!(record.level, AuditLevel::Info, "{}", reason.as_str());
            assert_eq!(record.error_type, None, "{}", reason.as_str());
            assert_eq!(record.error_message, None, "{}", reason.as_str());
        }
    }

    #[test]
    fn a_clean_close_is_not_reported_as_a_failure() {
        for reason in [
            CloseReason::UpstreamClosed,
            CloseReason::ClientDisconnected,
            CloseReason::Rejected,
        ] {
            assert!(!failed_close(reason), "{}", reason.as_str());
        }
        assert!(failed_close(CloseReason::Aborted));
        assert!(failed_close(CloseReason::IdleTimeout));
    }

    #[test]
    fn the_timestamp_is_utc_with_millisecond_precision() {
        let stamp = format_utc_millis(Duration::from_millis(1_794_387_612_345));
        assert_eq!(stamp, "2026-11-11T09:00:12.345Z");
        assert_eq!(
            format_utc_millis(Duration::from_millis(0)),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_utc_millis(Duration::from_millis(86_400_000)),
            "1970-01-02T00:00:00.000Z"
        );
        assert_eq!(
            format_utc_millis(Duration::from_millis(1_709_164_800_000)),
            "2024-02-29T00:00:00.000Z"
        );
        let now = timestamp_now();
        assert_eq!(now.len(), 24, "yyyy-mm-ddThh:mm:ss.mmmZ: {now}");
        assert!(now.ends_with('Z'), "{now}");
    }

    #[test]
    fn the_serialized_line_is_a_single_json_document_with_no_other_key() {
        let context = context();
        let record = AuditRecord::build(&input(&context));
        let line = record.to_line().expect("the line serializes");
        assert!(!line.contains('\n'), "one line, not several: {line}");
        assert!(redaction_findings(&line).is_empty(), "{line}");
    }

    #[test]
    fn the_redaction_check_reports_a_line_with_an_unrecorded_key() {
        let findings = redaction_findings(
            "{\"timestamp\":\"2026-09-06T00:00:00.000Z\",\"level\":\"INFO\",\"event\":\
             \"proxy_request\",\"request_id\":null,\"tenant_id\":null,\"principal_id\":null,\
             \"host\":null,\"path\":null,\"method\":null,\"status\":null,\"duration_ms\":null,\
             \"request_size\":null,\"response_size\":null,\"error_type\":null,\
             \"authorization\":\"Bearer x\"}",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("authorization"));
    }

    #[test]
    fn the_redaction_check_reports_a_query_string_in_the_path() {
        let findings = redaction_findings(
            "{\"timestamp\":\"2026-09-06T00:00:00.000Z\",\"level\":\"INFO\",\"event\":\
             \"proxy_request\",\"request_id\":null,\"tenant_id\":null,\"principal_id\":null,\
             \"host\":null,\"path\":\"/v1?token=1\",\"method\":null,\"status\":null,\
             \"duration_ms\":null,\"request_size\":null,\"response_size\":null,\
             \"error_type\":null}",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn a_context_with_no_identifier_records_no_empty_request_id() {
        let mut context = RequestContext::new(
            String::new(),
            "/oagw/v1/proxy/api.vendor.com/v1/things".to_owned(),
            "GET".to_owned(),
        );
        context.request_id = Some(String::new());
        assert_eq!(correlation_id(&context), None);
        context.request_id = Some("req-1".to_owned());
        assert_eq!(correlation_id(&context).as_deref(), Some("req-1"));
    }

    #[test]
    fn the_identifier_the_pipeline_opened_is_the_fallback() {
        // The transform recorded nothing, so the identifier the pipeline opened
        // the context with at `inst-pe-req-03` is the correlation identifier.
        let mut context = context();
        assert_eq!(correlation_id(&context).as_deref(), Some("trace-1"));
        context.request_id = Some("req-1".to_owned());
        assert_eq!(
            correlation_id(&context).as_deref(),
            Some("req-1"),
            "the transform-recorded identifier wins"
        );
    }

    #[test]
    fn the_level_serializes_as_its_recorded_name_and_no_other() {
        let levels = [
            (AuditLevel::Info, "INFO"),
            (AuditLevel::Warn, "WARN"),
            (AuditLevel::Error, "ERROR"),
        ];
        for (level, name) in levels {
            let line = serde_json::to_string(&level).expect("the level serializes");
            assert_eq!(line, format!("\"{name}\""));
            let read: AuditLevel = serde_json::from_str(&line).expect("the level reads back");
            assert_eq!(read, level);
        }
        assert!(serde_json::from_str::<AuditLevel>("\"TRACE\"").is_err());
    }
}
