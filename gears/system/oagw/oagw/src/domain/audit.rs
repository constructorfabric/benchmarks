//! ADR-0001 structured audit logging for the OAGW control plane.
//!
//! Every control-plane mutation (create, replace, delete) emits one JSON line
//! under the [`AUDIT_TARGET`] tracing target so a collector can pick the
//! records up from stdout regardless of how the subscriber formats plain
//! messages. The payload keeps the ADR-0001 field set:
//!
//! ```json
//! {
//!   "timestamp": "2026-02-03T11:09:37.431Z",
//!   "level": "INFO",
//!   "event": "upstream.create",
//!   "request_id": null,
//!   "tenant_id": "…",
//!   "principal_id": "…",
//!   "resource_type": "upstream",
//!   "resource_id": "gts.cf.core.oagw.upstream.v1~…",
//!   "alias": "api.openai.com",
//!   "error_type": null
//! }
//! ```
//!
//! The data plane emits the same shape for every proxied request, with the
//! DESIGN §4.3 request field set (see [`ProxyExchange`]).
//!
//! The gear has no `time`/`chrono` dependency, so [`format_rfc3339`] renders
//! timestamps from [`std::time::SystemTime`] directly (proleptic Gregorian
//! calendar, Howard Hinnant's `civil_from_days` algorithm).

use std::time::SystemTime;

use serde_json::{Map, Value};
use toolkit_security::SecurityContext;

/// `tracing` target every OAGW audit line is emitted under.
pub const AUDIT_TARGET: &str = "oagw.audit";

/// `level` field of an audit payload; the control plane only logs successful
/// mutations, so it is constant.
pub const AUDIT_LEVEL: &str = "INFO";

/// `level` value DESIGN §4.3 gives rate-limit rejections and circuit-breaker
/// openings.
pub const AUDIT_LEVEL_WARN: &str = "WARN";

/// `level` value DESIGN §4.3 gives upstream failures, timeouts and auth
/// failures.
pub const AUDIT_LEVEL_ERROR: &str = "ERROR";

/// `event` field of a proxied-request record.
pub const PROXY_EVENT: &str = "proxy.request";

/// Seconds per day.
const SECS_PER_DAY: i64 = 86_400;

/// Days from `0000-03-01` to `1970-01-01` (Hinnant's epoch shift).
const DAYS_TO_UNIX_EPOCH: i64 = 719_468;

/// Renders `time` as an RFC 3339 UTC timestamp with millisecond precision.
///
/// Pre-epoch instants render with a negative year rather than being clamped,
/// so a caller can never mistake a clamped value for a real one.
#[must_use]
pub fn format_rfc3339(time: SystemTime) -> String {
    let (secs, millis) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(delta) => (
            i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
            delta.subsec_millis(),
        ),
        Err(error) => {
            let delta = error.duration();
            let secs = i64::try_from(delta.as_secs()).unwrap_or(i64::MAX);
            let millis = delta.subsec_millis();
            if millis == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000 - millis)
            }
        }
    };

    let days = secs.div_euclid(SECS_PER_DAY);
    let secs_of_day = secs.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Converts a day count relative to the Unix epoch into `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + DAYS_TO_UNIX_EPOCH;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// Builds the ADR-0001 audit payload for one control-plane mutation.
///
/// Pure on purpose: [`log_mutation`] only adds the `tracing` emission, so unit
/// tests can assert on the payload itself.
#[must_use]
pub fn mutation_event(
    event: &str,
    ctx: &SecurityContext,
    resource_type: &str,
    resource_id: &str,
    request_id: Option<&str>,
    alias: Option<&str>,
    detail: Option<&str>,
) -> Value {
    let mut payload = Map::new();
    payload.insert(
        "timestamp".to_owned(),
        Value::String(format_rfc3339(SystemTime::now())),
    );
    payload.insert("level".to_owned(), Value::String(AUDIT_LEVEL.to_owned()));
    payload.insert("event".to_owned(), Value::String(event.to_owned()));
    payload.insert(
        "request_id".to_owned(),
        request_id.map_or(Value::Null, |id| Value::String(id.to_owned())),
    );
    payload.insert(
        "tenant_id".to_owned(),
        Value::String(ctx.subject_tenant_id().to_string()),
    );
    payload.insert(
        "principal_id".to_owned(),
        Value::String(ctx.subject_id().to_string()),
    );
    payload.insert(
        "resource_type".to_owned(),
        Value::String(resource_type.to_owned()),
    );
    payload.insert(
        "resource_id".to_owned(),
        Value::String(resource_id.to_owned()),
    );
    payload.insert(
        "alias".to_owned(),
        alias.map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
    payload.insert(
        "detail".to_owned(),
        detail.map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
    payload.insert("error_type".to_owned(), Value::Null);
    Value::Object(payload)
}

/// Emits the ADR-0001 audit line for a control-plane mutation.
pub fn log_mutation(
    event: &str,
    ctx: &SecurityContext,
    resource_type: &str,
    resource_id: &str,
    request_id: Option<&str>,
    alias: Option<&str>,
    detail: Option<&str>,
) {
    let payload = mutation_event(
        event,
        ctx,
        resource_type,
        resource_id,
        request_id,
        alias,
        detail,
    );
    tracing::info!(target: AUDIT_TARGET, event = %event, "{payload}");
}

/// Which side produced the answer of a proxied request, in the ADR-0007
/// `X-OAGW-Error-Source` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyOutcome {
    /// The gateway answered without an upstream answer.
    #[default]
    Gateway,
    /// The upstream answered.
    Upstream,
}

impl ProxyOutcome {
    /// The JSON spelling of the outcome.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// The DESIGN §4.3 request field set of one proxied request.
///
/// [`log_proxy`] renders it as one JSON line under [`AUDIT_TARGET`]; the
/// collector joins it with the control-plane records on `request_id`. No body,
/// query parameter, header or credential is ever recorded here (DESIGN §4.3
/// "No PII" / "No secrets"): `path` is the *outbound* path the route rewrite
/// produced, without its query string.
#[derive(Debug, Clone, Default)]
pub struct ProxyExchange {
    /// Correlation id of the request, echoed from the client or generated by a
    /// plugin.
    pub request_id: Option<String>,
    /// Tenant of the authenticated caller.
    pub tenant_id: Option<String>,
    /// Principal of the authenticated caller.
    pub principal_id: Option<String>,
    /// Proxy alias the request addressed.
    pub alias: String,
    /// Target host the route resolved to.
    pub host: String,
    /// Metric label of the matched route (see
    /// [`crate::domain::metrics::route_label`]); [`UNMATCHED_ROUTE`] when no
    /// route resolved.
    pub route: String,
    /// Upstream endpoint the request was actually sent to.
    pub endpoint: String,
    /// HTTP method of the request.
    pub method: String,
    /// Outbound path, after the route rewrite and without its query.
    pub outbound_path: String,
    /// Response status the client received.
    pub status: u16,
    /// Milliseconds from the request reaching the handler to the answer.
    pub duration_ms: u64,
    /// Bytes of the request body (`0` when it was not buffered).
    pub request_size: u64,
    /// Bytes of the response body as the upstream declared them (`0` when it
    /// declared none or streamed them).
    pub response_size: u64,
    /// Which side produced the answer.
    pub outcome: ProxyOutcome,
    /// GTS error type of a failed request.
    pub error_type: Option<String>,
    /// Human-readable message of a failed request.
    pub error_message: Option<String>,
}

impl ProxyExchange {
    /// Starts the record of one proxied request, before a route is known.
    #[must_use]
    pub fn new(alias: &str, method: &str) -> Self {
        Self {
            alias: alias.to_owned(),
            method: method.to_owned(),
            route: crate::domain::metrics::UNMATCHED_ROUTE.to_owned(),
            ..Self::default()
        }
    }

    /// The DESIGN §4.3 log level of the record.
    ///
    /// | Outcome | Status | Level |
    /// |---|---|---|
    /// | `upstream` | `5xx` | `ERROR` (§4.3 "upstream failures, timeouts") |
    /// | `upstream` | `2xx`/`3xx`/`4xx` | `INFO` (a normal operation: the
    ///   gateway forwarded it and the caller reads the status itself) |
    /// | `gateway` | `401`/`403` | `ERROR` (auth failure) |
    /// | `gateway` | `429` | `WARN` (rate limit exceeded) |
    /// | `gateway` | other `5xx` | `ERROR` (upstream failure, timeout) |
    /// | `gateway` | anything else | `INFO` (`404` route, `400` validation,
    ///   `204` preflight) |
    #[must_use]
    pub fn level(&self) -> &'static str {
        match self.status {
            401 | 403 => AUDIT_LEVEL_ERROR,
            429 => AUDIT_LEVEL_WARN,
            status if status >= 500 => AUDIT_LEVEL_ERROR,
            _ => AUDIT_LEVEL,
        }
    }
}

/// Builds the DESIGN §4.3 audit payload of one proxied request.
///
/// Pure on purpose: [`log_proxy`] only adds the `tracing` emission and the
/// level dispatch, so unit tests can assert on the payload itself.
#[must_use]
pub fn proxy_event(exchange: &ProxyExchange) -> Value {
    let mut payload = Map::new();
    payload.insert(
        "timestamp".to_owned(),
        Value::String(format_rfc3339(SystemTime::now())),
    );
    payload.insert(
        "level".to_owned(),
        Value::String(exchange.level().to_owned()),
    );
    payload.insert("event".to_owned(), Value::String(PROXY_EVENT.to_owned()));
    payload.insert(
        "request_id".to_owned(),
        exchange
            .request_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    payload.insert(
        "tenant_id".to_owned(),
        exchange
            .tenant_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    payload.insert(
        "principal_id".to_owned(),
        exchange
            .principal_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    payload.insert("alias".to_owned(), Value::String(exchange.alias.clone()));
    payload.insert("host".to_owned(), Value::String(exchange.host.clone()));
    payload.insert("route".to_owned(), Value::String(exchange.route.clone()));
    payload.insert(
        "endpoint".to_owned(),
        Value::String(exchange.endpoint.clone()),
    );
    payload.insert("method".to_owned(), Value::String(exchange.method.clone()));
    payload.insert(
        "path".to_owned(),
        Value::String(exchange.outbound_path.clone()),
    );
    payload.insert("status".to_owned(), Value::from(exchange.status));
    payload.insert("duration_ms".to_owned(), Value::from(exchange.duration_ms));
    payload.insert(
        "request_size".to_owned(),
        Value::from(exchange.request_size),
    );
    payload.insert(
        "response_size".to_owned(),
        Value::from(exchange.response_size),
    );
    payload.insert(
        "outcome".to_owned(),
        Value::String(exchange.outcome.as_str().to_owned()),
    );
    payload.insert(
        "error_type".to_owned(),
        exchange
            .error_type
            .clone()
            .map_or(Value::Null, Value::String),
    );
    payload.insert(
        "error_message".to_owned(),
        exchange
            .error_message
            .clone()
            .map_or(Value::Null, Value::String),
    );
    Value::Object(payload)
}

/// Emits the DESIGN §4.3 audit line of one proxied request, at the level the
/// design assigns to its outcome.
pub fn log_proxy(exchange: &ProxyExchange) {
    let payload = proxy_event(exchange);
    match exchange.level() {
        AUDIT_LEVEL_WARN => {
            tracing::warn!(target: AUDIT_TARGET, event = PROXY_EVENT, "{payload}")
        }
        AUDIT_LEVEL_ERROR => {
            tracing::error!(target: AUDIT_TARGET, event = PROXY_EVENT, "{payload}")
        }
        _ => tracing::info!(target: AUDIT_TARGET, event = PROXY_EVENT, "{payload}"),
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
