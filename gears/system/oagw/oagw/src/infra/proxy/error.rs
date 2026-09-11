// Updated: 2026-09-01 by Constructor Tech
//! The gateway's own error vocabulary.
//!
//! Every error the Data Plane produces is an RFC 9457 problem document whose
//! `type` is one of the OAGW GTS error identifiers in [`crate::gts`]. This is
//! the single type that carries them onto the wire, for the proxy and for the
//! management API alike.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::domain::error::DomainError;
use crate::gts;

/// The `X-OAGW-Error-Source` values (ADR-0007).
pub const SOURCE_GATEWAY: &str = "gateway";
pub const SOURCE_UPSTREAM: &str = "upstream";

/// An error the gateway produces itself.
#[derive(Debug, Clone)]
pub struct GatewayError {
    pub status: StatusCode,
    pub type_id: &'static str,
    pub detail: String,
    /// ADR-0007: `gateway` for everything this type can express.
    pub source: &'static str,
    pub extra: Vec<(&'static str, Value)>,
}

impl GatewayError {
    #[must_use]
    pub fn new(status: StatusCode, type_id: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            type_id,
            detail: detail.into(),
            source: SOURCE_GATEWAY,
            extra: Vec::new(),
        }
    }

    /// Attach a structured extension member to the problem document.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: Value) -> Self {
        self.extra.push((key, value));
        self
    }

    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, gts::ERR_VALIDATION, detail)
    }

    #[must_use]
    pub fn validation_issue(field: &str, message: &str) -> Self {
        Self::validation(format!("{field}: {message}")).with("field", json!(field))
    }

    #[must_use]
    pub fn not_found_resource(kind: &str, id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            gts::ERR_RESOURCE_NOT_FOUND,
            format!("{kind} '{id}' was not found"),
        )
        .with("resource_id", json!(id))
    }

    #[must_use]
    pub fn upstream_not_found(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            gts::ERR_UPSTREAM_NOT_FOUND,
            format!("upstream '{id}' was not found"),
        )
        .with("upstream_id", json!(id))
    }

    #[must_use]
    pub fn route_not_found(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            gts::ERR_ROUTE_NOT_FOUND,
            format!("route '{id}' was not found"),
        )
        .with("route_id", json!(id))
    }

    /// No route serves this alias and path.
    #[must_use]
    pub fn no_route(alias: &str, path: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            gts::ERR_ROUTE_NOT_FOUND,
            format!("no route on upstream '{alias}' matches '{path}'"),
        )
        .with("alias", json!(alias))
        .with("path", json!(path))
    }

    /// No upstream answers this alias in the caller's tenant chain.
    #[must_use]
    pub fn unknown_alias(alias: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            gts::ERR_UPSTREAM_NOT_FOUND,
            format!("no upstream is registered for the alias '{alias}'"),
        )
        .with("alias", json!(alias))
    }

    /// `X-OAGW-Target-Host` is required to name one endpoint of a pool.
    #[must_use]
    pub fn missing_target_host(alias: &str, valid_hosts: &[String]) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            gts::ERR_MISSING_TARGET_HOST,
            format!(
                "X-OAGW-Target-Host header required for multi-endpoint upstream with common \
                 suffix alias. Valid hosts: [{}]",
                valid_hosts.join(", ")
            ),
        )
        .with("alias", json!(alias))
        .with("valid_hosts", json!(valid_hosts))
    }

    /// `X-OAGW-Target-Host` is not a bare hostname or IP literal.
    #[must_use]
    pub fn invalid_target_host(value: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            gts::ERR_INVALID_TARGET_HOST,
            "X-OAGW-Target-Host must be a valid hostname or IP address \
             (no port, path, or special characters)",
        )
        .with("invalid_value", json!(value))
    }

    /// `X-OAGW-Target-Host` names no endpoint the upstream configures.
    #[must_use]
    pub fn unknown_target_host(value: &str, valid_hosts: &[String]) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            gts::ERR_UNKNOWN_TARGET_HOST,
            format!(
                "X-OAGW-Target-Host '{value}' does not match any configured endpoint. \
                 Valid hosts: [{}]",
                valid_hosts.join(", ")
            ),
        )
        .with("invalid_value", json!(value))
        .with("valid_hosts", json!(valid_hosts))
    }

    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, gts::ERR_RESOURCE_CONFLICT, detail)
    }

    #[must_use]
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, gts::ERR_FORBIDDEN, detail)
    }

    /// A plugin in the chain refused the exchange.
    ///
    /// The GTS type follows the status the plugin chose, so a problem document
    /// never reports a type and a status that disagree: a request-phase guard
    /// rejection is a validation failure (ADR-0009 pins it at 400), and a
    /// response-phase one refuses the upstream's answer (pinned at 502). The
    /// plugin's own code — `REQUIRED_HEADER_MISSING`, say — travels as an
    /// extension member, because the type names the family and the code names
    /// the rule the caller broke.
    #[must_use]
    pub fn plugin_rejected(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        let type_id = if status.is_client_error() {
            gts::ERR_VALIDATION
        } else {
            gts::ERR_PROTOCOL_ERROR
        };
        Self::new(status, type_id, message).with("plugin_code", json!(code))
    }

    #[must_use]
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, gts::ERR_AUTH_FAILED, detail)
    }

    #[must_use]
    pub fn payload_too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            gts::ERR_PAYLOAD_TOO_LARGE,
            format!("request body exceeds the configured limit of {limit} bytes"),
        )
        .with("max_payload_bytes", json!(limit))
    }

    #[must_use]
    pub fn rate_limited(retry_after: u64, limit: u64, remaining: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            gts::ERR_RATE_LIMIT_EXCEEDED,
            "rate limit exceeded for this scope",
        )
        .with("retry_after_seconds", json!(retry_after))
        .with("rate_limit", json!(limit))
        .with("rate_remaining", json!(remaining))
    }

    #[must_use]
    pub fn circuit_open(upstream_id: &str, open_duration_secs: u64) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_CIRCUIT_BREAKER_OPEN,
            format!("upstream '{upstream_id}' is unavailable: circuit breaker open"),
        )
        .with("upstream_id", json!(upstream_id))
        .with("retry_after_seconds", json!(open_duration_secs))
    }

    #[must_use]
    pub fn bad_gateway(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, gts::ERR_DOWNSTREAM_ERROR, detail)
    }

    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_LINK_UNAVAILABLE,
            detail,
        )
    }

    #[must_use]
    pub fn connect_timeout(host: &str) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_CONNECTION_TIMEOUT,
            format!("connecting to '{host}' timed out"),
        )
        .with("host", json!(host))
    }

    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_REQUEST_TIMEOUT,
            detail,
        )
    }

    #[must_use]
    pub fn cors_origin(origin: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            format!("origin '{origin}' is not allowed"),
        )
        .with("origin", json!(origin))
    }

    #[must_use]
    pub fn cors_method(method: &str, origin: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            gts::ERR_CORS_METHOD_NOT_ALLOWED,
            format!("method '{method}' is not allowed for origin '{origin}'"),
        )
        .with("method", json!(method))
        .with("origin", json!(origin))
    }

    #[must_use]
    pub fn secret_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_SECRET_NOT_FOUND,
            detail,
        )
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, gts::ERR_INTERNAL, detail)
    }

    /// The problem document body, ready to serialize.
    #[must_use]
    pub fn body(&self) -> Value {
        let mut doc = json!({
            "type": self.type_id,
            "title": gts::error_title(self.type_id),
            "status": self.status.as_u16(),
            "detail": self.detail,
        });
        if let Some(obj) = doc.as_object_mut() {
            for (key, value) in &self.extra {
                obj.insert((*key).to_owned(), value.clone());
            }
        }
        doc
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.status.as_u16(),
            self.type_id,
            self.detail
        )
    }
}

impl std::error::Error for GatewayError {}

/// The ADR-0007 header name.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(SOURCE_GATEWAY),
        );
        if let Ok(v) = HeaderValue::from_str(self.type_id) {
            headers.insert("x-oagw-error-type", v);
        }
        // Any `retry_after_seconds` extension is also a real header.
        if let Some(Value::Number(n)) = self
            .extra
            .iter()
            .find(|(k, _)| *k == "retry_after_seconds")
            .map(|(_, v)| v)
            && let Ok(v) = HeaderValue::from_str(&n.to_string())
        {
            headers.insert(header::RETRY_AFTER, v);
        }
        // A refused request still reports the budget it would have spent
        // (ADR-0003): the caller learns the ceiling and how much of it is left.
        if let Some(Value::Number(n)) = self
            .extra
            .iter()
            .find(|(k, _)| *k == "rate_limit")
            .map(|(_, v)| v)
            && let Ok(v) = HeaderValue::from_str(&n.to_string())
        {
            headers.insert(header::HeaderName::from_static("x-ratelimit-limit"), v);
        }
        if let Some(Value::Number(n)) = self
            .extra
            .iter()
            .find(|(k, _)| *k == "rate_remaining")
            .map(|(_, v)| v)
            && let Ok(v) = HeaderValue::from_str(&n.to_string())
        {
            headers.insert(header::HeaderName::from_static("x-ratelimit-remaining"), v);
        }
        let mut response = (self.status, axum::Json(self.body())).into_response();
        response.headers_mut().extend(headers);
        response
    }
}

impl From<DomainError> for GatewayError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::Validation(issues) => {
                let mut e = Self::validation("the request failed validation");
                e.extra = issues
                    .iter()
                    .map(|i| ("issues", json!({"field": i.field, "message": i.message})))
                    .collect();
                e
            }
            DomainError::InvalidField { field, message } => {
                Self::validation_issue(&field, &message)
            }
            DomainError::NotFound { kind, id } => Self::not_found_resource(kind, &id),
            DomainError::Conflict { message, .. } => Self::conflict(message),
            DomainError::PluginInUse {
                plugin_id,
                referenced_by,
            } => Self::new(
                StatusCode::CONFLICT,
                gts::ERR_PLUGIN_IN_USE,
                format!("plugin '{plugin_id}' is still referenced"),
            )
            .with("plugin_id", json!(plugin_id))
            .with(
                "referenced_by",
                json!({
                    "upstreams": referenced_by.upstreams,
                    "routes": referenced_by.routes,
                }),
            ),
            DomainError::PluginAlreadyExists(id) => {
                Self::conflict(format!("plugin '{id}' already exists"))
            }
            DomainError::NoTenant => Self::unauthorized("the request carries no tenant identity"),
            DomainError::Forbidden => Self::forbidden("the caller may not perform this action"),
            DomainError::BadIdentifier(raw) => {
                Self::validation(format!("'{raw}' is not a valid resource identifier"))
                    .with("invalid_value", json!(raw))
            }
            DomainError::Internal(m) => Self::internal(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_carries_the_gts_type_and_title() {
        let e = GatewayError::unknown_alias("api.example.com");
        let b = e.body();
        // An alias no upstream is stored under is a missing resource, not a
        // bad `X-OAGW-Target-Host` header — that identifier is for a target
        // host that does not match an upstream that *was* resolved.
        assert_eq!(b["type"], gts::ERR_UPSTREAM_NOT_FOUND);
        assert_eq!(b["title"], "Upstream Not Found");
        assert_eq!(b["status"], 404);
        assert_eq!(b["alias"], "api.example.com");
    }

    #[test]
    fn rate_limit_error_sets_the_budget_headers_on_the_wire() {
        let e = GatewayError::rate_limited(7, 100, 0);
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "7");
        assert_eq!(
            resp.headers().get(ERROR_SOURCE_HEADER).unwrap(),
            SOURCE_GATEWAY
        );
        // ADR-0003: the refusal carries the budget too, so the caller can tell
        // a full bucket from a half-spent one.
        assert_eq!(resp.headers().get("x-ratelimit-limit").unwrap(), "100");
        assert_eq!(resp.headers().get("x-ratelimit-remaining").unwrap(), "0");
        assert!(resp.headers().get("x-ratelimit-reset").is_none());
    }

    #[test]
    fn a_guard_rejection_is_a_validation_failure_not_an_auth_one() {
        // ADR-0009 pins the request phase at 400 with
        // `REQUIRED_HEADER_MISSING`. `auth.failed.v1` is for an upstream
        // credential the gateway could not establish, which is not what
        // happened: the caller never got as far as an upstream.
        let e = GatewayError::plugin_rejected(
            StatusCode::BAD_REQUEST,
            "REQUIRED_HEADER_MISSING",
            "REQUIRED_HEADER_MISSING:x-must-be-here",
        );
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.type_id, gts::ERR_VALIDATION);
        assert_eq!(e.body()["title"], "Validation Error");
        assert_eq!(e.body()["plugin_code"], "REQUIRED_HEADER_MISSING");
    }

    #[test]
    fn a_response_phase_rejection_reports_a_protocol_error() {
        // The response phase is pinned at 502 (ADR-0009): the upstream's answer
        // is the thing that failed the contract.
        let e = GatewayError::plugin_rejected(
            StatusCode::BAD_GATEWAY,
            "REQUIRED_HEADER_MISSING",
            "REQUIRED_HEADER_MISSING:content-type",
        );
        assert_eq!(e.type_id, gts::ERR_PROTOCOL_ERROR);
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn domain_errors_map_onto_gateway_errors() {
        let e: GatewayError = DomainError::NotFound {
            kind: "upstream",
            id: "abc".into(),
        }
        .into();
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert_eq!(e.type_id, gts::ERR_RESOURCE_NOT_FOUND);

        let e: GatewayError = DomainError::Forbidden.into();
        assert_eq!(e.status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn plugin_in_use_maps_onto_its_own_type() {
        let e: GatewayError = DomainError::PluginInUse {
            plugin_id: "p-1".into(),
            referenced_by: crate::domain::error::ReferencedBy {
                upstreams: vec!["u-1".into()],
                routes: vec![],
            },
        }
        .into();
        assert_eq!(e.type_id, gts::ERR_PLUGIN_IN_USE);
        assert_eq!(e.body()["referenced_by"]["upstreams"][0], "u-1");
    }
}
