//! Error response mapping (`DomainError` → RFC 9457 problem document).
//!
//! Every gateway error is an `application/problem+json` document whose `type`
//! is the GTS problem-type id from the DESIGN.md error table, and which
//! carries **top-level** extension members for the fields a client needs to
//! act on (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`,
//! plus `plugin_id`/`referenced_by` for the plugin-in-use conflict and
//! `violations` for validation failures). The `context` member the platform's
//! canonical error envelope requires mirrors those same facts as one object.
//!
//! The response always carries `X-OAGW-Error-Source` (`gateway` or
//! `upstream`) and, when the error advertises one, a `Retry-After` header.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::domain::error::{DomainError, ErrorSource, FieldViolation};

/// Header naming which side produced an error.
pub const OAGW_ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// Content type of a problem document.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// Wire problem document for OAGW errors.
#[derive(Debug, Clone, PartialEq)]
pub struct OagwProblem {
    /// GTS problem type id.
    pub problem_type: String,
    /// Human-readable summary.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Human-readable explanation.
    pub detail: String,
    /// URI reference identifying the occurrence.
    pub instance: Option<String>,
    /// Trace / request id.
    pub trace_id: Option<String>,
    /// GTS instance id of the upstream involved, when known.
    pub upstream_id: Option<String>,
    /// Target host, when known.
    pub host: Option<String>,
    /// Request path, when known.
    pub path: Option<String>,
    /// Resolved alias, when known.
    pub alias: Option<String>,
    /// Offending value, when the error is about one.
    pub invalid_value: Option<String>,
    /// Hosts that would have been acceptable, when known.
    pub valid_hosts: Option<Vec<String>>,
    /// Plugin that is still referenced, for `plugin.in_use`.
    pub plugin_id: Option<String>,
    /// Resources still referencing the plugin, for `plugin.in_use`.
    pub referenced_by: Option<crate::domain::error::PluginReferences>,
    /// Field-level validation failures, for `validation.error`.
    pub violations: Vec<FieldViolation>,
    /// Suggested retry delay in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Which side produced the error.
    pub error_source: ErrorSource,
}

impl OagwProblem {
    /// Build a problem from the parts every error carries.
    #[must_use]
    pub fn new(status: u16, problem_type: &str, title: &str, detail: impl Into<String>) -> Self {
        Self {
            problem_type: problem_type.to_owned(),
            title: title.to_owned(),
            status,
            detail: detail.into(),
            instance: None,
            trace_id: None,
            upstream_id: None,
            host: None,
            path: None,
            alias: None,
            invalid_value: None,
            valid_hosts: None,
            plugin_id: None,
            referenced_by: None,
            violations: Vec::new(),
            retry_after_seconds: None,
            error_source: ErrorSource::Gateway,
        }
    }

    /// Attach a trace id.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: Option<String>) -> Self {
        self.trace_id = trace_id;
        self
    }

    /// Attach the occurrence URI.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Attach the resolved upstream id (GTS instance id form).
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    /// Attach the target host.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Attach the proxied path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Mark the error as produced by the upstream.
    #[must_use]
    pub const fn with_upstream_source(mut self) -> Self {
        self.error_source = ErrorSource::Upstream;
        self
    }

    /// Serialise to the wire body.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut body = json!({
            "type": self.problem_type,
            "title": self.title,
            "status": self.status,
            "detail": self.detail,
        });
        let Some(object) = body.as_object_mut() else {
            return body;
        };
        let optional_str = |object: &mut serde_json::Map<String, serde_json::Value>,
                            key: &str,
                            value: &Option<String>| {
            if let Some(value) = value {
                object.insert(key.to_owned(), json!(value));
            }
        };
        optional_str(object, "instance", &self.instance);
        optional_str(object, "trace_id", &self.trace_id);
        optional_str(object, "upstream_id", &self.upstream_id);
        optional_str(object, "host", &self.host);
        optional_str(object, "path", &self.path);
        optional_str(object, "alias", &self.alias);
        optional_str(object, "invalid_value", &self.invalid_value);
        optional_str(object, "plugin_id", &self.plugin_id);
        if let Some(valid_hosts) = &self.valid_hosts {
            object.insert("valid_hosts".to_owned(), json!(valid_hosts));
        }
        if let Some(referenced_by) = &self.referenced_by {
            object.insert("referenced_by".to_owned(), json!(referenced_by));
        }
        if let Some(retry) = self.retry_after_seconds {
            object.insert("retry_after_seconds".to_owned(), json!(retry));
        }
        if !self.violations.is_empty() {
            object.insert(
                "violations".to_owned(),
                json!(
                    self.violations
                        .iter()
                        .map(|v| json!({"field": v.field, "detail": v.detail}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        // The platform's canonical error envelope
        // (`toolkit_canonical_errors::Problem`) requires a `context` member and
        // fails to deserialize a problem document without one, which would cost
        // every gateway error its `instance` / `trace_id` injection and its
        // structured `log_problem` entry. The OAGW-specific members above stay
        // at the top level — DESIGN.md names them there — and `context` mirrors
        // them so the same facts survive a projection onto the platform type.
        object.insert("context".to_owned(), self.context());
        body
    }

    /// The platform `context` member: every OAGW extension member that is
    /// present, in one object. Always an object, mirroring
    /// `toolkit_canonical_errors`' own `serialize_context`.
    fn context(&self) -> serde_json::Value {
        let mut context = serde_json::Map::new();
        let mut put = |key: &str, value: serde_json::Value| {
            if !value.is_null() {
                context.insert(key.to_owned(), value);
            }
        };
        put("upstream_id", json!(self.upstream_id));
        put("host", json!(self.host));
        put("path", json!(self.path));
        put("alias", json!(self.alias));
        put("invalid_value", json!(self.invalid_value));
        put("plugin_id", json!(self.plugin_id));
        put("valid_hosts", json!(self.valid_hosts));
        put("referenced_by", json!(self.referenced_by));
        put("retry_after_seconds", json!(self.retry_after_seconds));
        if !self.violations.is_empty() {
            put(
                "violations",
                json!(
                    self.violations
                        .iter()
                        .map(|v| json!({"field": v.field, "detail": v.detail}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        serde_json::Value::Object(context)
    }

    /// HTTP status as an axum status code.
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl From<DomainError> for OagwProblem {
    fn from(err: DomainError) -> Self {
        use DomainError as E;
        let mut problem = OagwProblem::new(
            err.status(),
            err.problem_type(),
            err.title(),
            detail_of(&err),
        );
        problem.error_source = err.source();
        problem.retry_after_seconds = err.retry_after().map(|duration| duration.as_secs());
        match err {
            E::Validation { violations, .. } => problem.violations = violations,
            E::MissingTargetHost { upstream_id, alias } => {
                problem.upstream_id = Some(upstream_id.to_string());
                problem.alias = Some(alias);
            }
            E::InvalidTargetHost {
                upstream_id,
                invalid_value,
                valid_hosts,
            }
            | E::UnknownTargetHost {
                upstream_id,
                invalid_value,
                valid_hosts,
            } => {
                problem.upstream_id = Some(upstream_id.to_string());
                problem.invalid_value = Some(invalid_value);
                problem.valid_hosts = (!valid_hosts.is_empty()).then_some(valid_hosts);
            }
            E::AliasImmutable { .. } | E::UpstreamIdImmutable { .. } => {}
            E::AuthenticationFailed { .. } => {}
            E::UpstreamNotFound { id } => problem.invalid_value = Some(id.to_string()),
            E::RouteNotFound { .. } => {}
            E::PluginNotFound { id } => problem.invalid_value = Some(id.to_string()),
            E::AliasConflict { alias } => problem.invalid_value = Some(alias),
            E::RouteMatchConflict { .. } => {}
            E::PluginInUse {
                plugin_id,
                referenced_by,
            } => {
                problem.plugin_id = Some(plugin_id);
                problem.referenced_by = Some(referenced_by);
            }
            E::PayloadTooLarge { limit_bytes } => {
                problem.detail =
                    format!("request body exceeds the configured limit of {limit_bytes} bytes");
            }
            E::RateLimitExceeded { .. } => {}
            E::SecretNotFound { .. } | E::Internal { .. } => {}
            E::ProtocolError { .. } | E::StreamAborted { .. } => {
                problem = problem.with_upstream_source();
            }
            E::DownstreamError {
                upstream_id,
                host,
                path,
                ..
            } => {
                // A `502 DownstreamError` is a problem document the gateway
                // synthesised (ADR 0007), so it keeps the default `gateway`
                // source; the upstream never produced a response to pass
                // through.
                problem = problem
                    .with_upstream_id_option(upstream_id.map(|u| u.to_string()))
                    .with_host_option(host)
                    .with_path_option(path);
            }
            E::LinkUnavailable { .. } | E::CircuitBreakerOpen { .. } => {}
            E::UpstreamDisabled { upstream_id, host } => {
                problem.upstream_id = Some(upstream_id.to_string());
                problem.host = host;
            }
            E::PluginUnavailable { .. } => {}
            E::ConnectionTimeout { .. } | E::IdleTimeout { .. } => {}
            E::RequestTimeout {
                upstream_id,
                host,
                path,
                ..
            } => {
                problem.upstream_id = upstream_id.map(|u| u.to_string());
                problem.host = host;
                problem.path = path;
            }
            E::MethodNotAllowed { method, allow } => {
                problem.path = Some(method);
                problem.alias = Some(allow);
            }
            E::CorsOriginNotAllowed { .. } | E::CorsMethodNotAllowed { .. } => {}
        }
        problem
    }
}

impl OagwProblem {
    /// Chainable setter used by the [`From<DomainError>`] mapping.
    fn with_upstream_id_option(mut self, upstream_id: Option<String>) -> Self {
        self.upstream_id = upstream_id;
        self
    }

    /// Chainable setter used by the [`From<DomainError>`] mapping.
    fn with_host_option(mut self, host: Option<String>) -> Self {
        self.host = host;
        self
    }

    /// Chainable setter used by the [`From<DomainError>`] mapping.
    fn with_path_option(mut self, path: Option<String>) -> Self {
        self.path = path;
        self
    }
}

/// Human-readable detail of a domain error, without echoing secrets.
fn detail_of(err: &DomainError) -> String {
    err.to_string()
}

/// The `Result` alias used by every OAGW handler.
///
/// Shadows the toolkit's `ApiResult` (which returns the platform's
/// `CanonicalError`) because OAGW owns its own problem-document shape.
pub type ApiResult<T, E = OagwProblem> = Result<T, E>;

impl IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.to_json().to_string();
        let mut headers = vec![
            (
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/problem+json"),
            ),
            (
                axum::http::HeaderName::from_static("x-oagw-error-source"),
                HeaderValue::from_static(match self.error_source {
                    ErrorSource::Gateway => "gateway",
                    ErrorSource::Upstream => "upstream",
                }),
            ),
        ];
        if let Some(retry_after) = self.retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
        {
            headers.push((axum::http::header::RETRY_AFTER, value));
        }
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = status;
        for (name, value) in headers {
            response.headers_mut().insert(name, value);
        }
        response
    }
}

/// OAGW resource marker for the platform's canonical error ladder.
///
/// Handlers return [`OagwProblem`] (OAGW owns its problem documents), but the
/// canonical projection is required so the platform's
/// `canonical_error_middleware` and any `ClientHub` consumer see a consistent
/// AIP-193 envelope when an OAGW error crosses a toolkit boundary.
#[toolkit_canonical_errors::resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub(crate) struct OagwResource;

/// Project a [`DomainError`] onto the platform's AIP-193 ladder.
impl From<DomainError> for toolkit_canonical_errors::CanonicalError {
    fn from(err: DomainError) -> Self {
        use DomainError as E;
        use toolkit_canonical_errors::CanonicalError;

        fn invalid(detail: impl Into<String>) -> CanonicalError {
            OagwResource::invalid_argument()
                .with_format(detail)
                .create()
        }
        fn not_found(detail: impl Into<String>) -> CanonicalError {
            OagwResource::not_found(detail)
                .with_resource("cf.core.oagw.tenant.v1~")
                .create()
        }
        fn internal(detail: impl Into<String>) -> CanonicalError {
            CanonicalError::internal(detail).create()
        }
        fn unavailable(detail: impl Into<String>) -> CanonicalError {
            CanonicalError::service_unavailable()
                .with_detail(detail)
                .create()
        }
        fn timeout(detail: impl Into<String>) -> CanonicalError {
            OagwResource::deadline_exceeded(detail).create()
        }
        fn upstream(detail: impl Into<String>) -> CanonicalError {
            OagwResource::unknown(detail).create()
        }

        match err {
            E::Validation { violations, detail } => {
                // The builder typestate allows field violations **or** a format
                // message, not both, so the richer of the two wins.
                if violations.is_empty() {
                    OagwResource::invalid_argument()
                        .with_format(detail)
                        .create()
                } else {
                    let mut builder = OagwResource::invalid_argument().with_field_violation(
                        violations[0].field.clone(),
                        violations[0].detail.clone(),
                        "OAGW_VALIDATION",
                    );
                    for violation in &violations[1..] {
                        builder = builder.with_field_violation(
                            violation.field.clone(),
                            violation.detail.clone(),
                            "OAGW_VALIDATION",
                        );
                    }
                    builder.create()
                }
            }
            E::MissingTargetHost { upstream_id, alias } => invalid(format!(
                "upstream {upstream_id} has no endpoint for alias {alias}"
            )),
            E::InvalidTargetHost { invalid_value, .. }
            | E::UnknownTargetHost { invalid_value, .. }
            | E::AliasImmutable {
                detail: invalid_value,
            }
            | E::UpstreamIdImmutable {
                detail: invalid_value,
            } => invalid(invalid_value),
            E::MethodNotAllowed { method, allow } => invalid(format!(
                "method {method} is not served by this route (allowed: {allow})"
            )),
            E::AuthenticationFailed { detail } => CanonicalError::unauthenticated()
                .with_reason(detail)
                .create(),
            E::UpstreamNotFound { id } => not_found(format!("upstream {id} not found")),
            E::RouteNotFound { detail } => not_found(detail),
            E::PluginNotFound { id } => not_found(format!("plugin {id} not found")),
            E::AliasConflict { alias } => OagwResource::already_exists(format!(
                "alias {alias} is already in use in this tenant"
            ))
            .with_resource("cf.core.oagw.tenant.v1~")
            .create(),
            E::RouteMatchConflict { detail } => OagwResource::aborted(detail)
                .with_reason("ROUTE_MATCH_CONFLICT")
                .create(),
            E::PluginInUse {
                plugin_id,
                referenced_by,
            } => OagwResource::already_exists(format!(
                "plugin {plugin_id} is referenced by {} upstream(s) and {} route(s)",
                referenced_by.upstreams.len(),
                referenced_by.routes.len()
            ))
            .with_resource("cf.core.oagw.plugin.v1~")
            .create(),
            E::PayloadTooLarge { limit_bytes } => invalid(format!(
                "request payload exceeds the configured limit of {limit_bytes} bytes"
            )),
            E::RateLimitExceeded {
                retry_after_seconds,
            } => CanonicalError::service_unavailable()
                .with_retry_after_seconds(retry_after_seconds)
                .with_detail("rate limit exceeded")
                .create(),
            E::SecretNotFound { detail } => internal(detail),
            E::Internal { diagnostic } => internal(diagnostic),
            E::DownstreamError { detail, .. } | E::StreamAborted { detail } => upstream(detail),
            E::ProtocolError { detail } => internal(detail),
            E::LinkUnavailable { detail, .. }
            | E::PluginUnavailable { detail }
            | E::CircuitBreakerOpen { detail, .. } => unavailable(detail),
            E::UpstreamDisabled { upstream_id, .. } => unavailable(format!(
                "upstream {upstream_id} is administratively disabled"
            )),
            E::ConnectionTimeout { detail, .. }
            | E::RequestTimeout { detail, .. }
            | E::IdleTimeout { detail, .. } => timeout(detail),
            E::CorsOriginNotAllowed { detail } | E::CorsMethodNotAllowed { detail } => {
                OagwResource::permission_denied()
                    .with_reason(detail)
                    .create()
            }
        }
    }
}

/// Serialisable view used by tests and by the OpenAPI description of errors.
impl OagwProblem {
    /// Serialise the problem document (same shape as the wire body).
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        self.to_json()
    }
}

/// Field violations are part of the problem document.
impl OagwProblem {
    /// Attach field-level violations.
    #[must_use]
    pub fn with_violations(mut self, violations: Vec<FieldViolation>) -> Self {
        self.violations = violations;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::problem;
    use std::time::Duration;

    #[test]
    fn statuses_follow_the_design_table() {
        let cases: [(DomainError, u16, &str); 8] = [
            (DomainError::invalid("bad"), 400, problem::VALIDATION_ERROR),
            (
                DomainError::AuthenticationFailed {
                    detail: "no".to_owned(),
                },
                401,
                problem::AUTHENTICATION_FAILED,
            ),
            (
                DomainError::AliasConflict {
                    alias: "a.com".to_owned(),
                },
                409,
                problem::ALIAS_CONFLICT,
            ),
            (
                DomainError::RateLimitExceeded {
                    retry_after_seconds: 5,
                },
                429,
                problem::RATE_LIMIT_EXCEEDED,
            ),
            (DomainError::internal("boom"), 500, problem::INTERNAL),
            (
                DomainError::ProtocolError {
                    detail: "x".to_owned(),
                },
                502,
                problem::PROTOCOL_ERROR,
            ),
            (
                DomainError::LinkUnavailable {
                    detail: "x".to_owned(),
                    retry_after_seconds: None,
                },
                503,
                problem::LINK_UNAVAILABLE,
            ),
            (
                DomainError::ConnectionTimeout {
                    detail: "x".to_owned(),
                    retry_after_seconds: None,
                },
                504,
                problem::CONNECTION_TIMEOUT,
            ),
        ];
        for (err, status, problem_type) in cases {
            let body = OagwProblem::from(err.clone());
            assert_eq!(body.status, status, "{err:?}");
            assert_eq!(body.problem_type, problem_type);
        }
    }

    /// The platform's canonical error middleware deserializes every
    /// problem+json response into `toolkit_canonical_errors::Problem`, which
    /// requires the `context` member. A document without it costs the error its
    /// `instance` / `trace_id` injection and its structured log entry.
    #[test]
    fn a_problem_document_parses_as_the_platform_problem_type() {
        use toolkit_canonical_errors::Problem;

        let problems = [
            OagwProblem::from(DomainError::RouteNotFound {
                detail: "no route".to_owned(),
            }),
            OagwProblem::from(DomainError::RateLimitExceeded {
                retry_after_seconds: 3,
            }),
            OagwProblem::from(DomainError::PluginInUse {
                plugin_id: "gts.cf.core.oagw.guard_plugin.v1~abc".to_owned(),
                referenced_by: crate::domain::error::PluginReferences::default(),
            }),
            OagwProblem::from(DomainError::MissingTargetHost {
                upstream_id: uuid::Uuid::new_v4(),
                alias: "api.openai.com".to_owned(),
            }),
            OagwProblem::from(DomainError::invalid("a field is wrong")),
        ];
        for problem in problems {
            let body = problem.to_json().to_string();
            let parsed: Problem = serde_json::from_str(&body)
                .unwrap_or_else(|err| panic!("{body} does not parse: {err}"));
            assert_eq!(parsed.status, problem.status);
            assert_eq!(parsed.problem_type, problem.problem_type);
            // The OAGW extension members stay at the top level, and `context`
            // mirrors them as one object.
            assert!(parsed.context.is_object());
        }
    }

    #[test]
    fn context_mirrors_the_extension_members_it_has() {
        let mut problem = OagwProblem::from(DomainError::RateLimitExceeded {
            retry_after_seconds: 9,
        });
        problem = problem.with_host("api.openai.com".to_owned());
        let body = problem.to_json();
        assert_eq!(body["retry_after_seconds"], 9);
        assert_eq!(body["context"]["retry_after_seconds"], 9);
        assert_eq!(body["context"]["host"], "api.openai.com");
        // Nothing is invented for a member the error does not carry.
        assert!(body["context"].get("upstream_id").is_none());
        // A bare problem still has an (empty) object, never a missing member.
        assert!(
            OagwProblem::from(DomainError::invalid("x")).to_json()["context"]
                .as_object()
                .is_some_and(|context| context.is_empty())
        );
    }

    #[test]
    fn plugin_in_use_carries_top_level_extensions() {
        let problem = OagwProblem::from(DomainError::PluginInUse {
            plugin_id: "gts.cf.core.oagw.guard_plugin.v1~abc".to_owned(),
            referenced_by: crate::domain::error::PluginReferences {
                upstreams: vec!["gts.cf.core.oagw.upstream.v1~1".to_owned()],
                routes: vec!["gts.cf.core.oagw.route.v1~2".to_owned()],
            },
        });
        let body = problem.to_json();
        assert_eq!(body["type"], problem::PLUGIN_IN_USE);
        assert_eq!(body["status"], 409);
        assert_eq!(body["plugin_id"], "gts.cf.core.oagw.guard_plugin.v1~abc");
        assert_eq!(
            body["referenced_by"]["upstreams"][0],
            "gts.cf.core.oagw.upstream.v1~1"
        );
    }

    #[test]
    fn missing_target_host_uses_the_routing_namespace() {
        let body = OagwProblem::from(DomainError::MissingTargetHost {
            upstream_id: uuid::Uuid::new_v4(),
            alias: "api.openai.com".to_owned(),
        })
        .to_json();
        assert_eq!(body["type"], problem::MISSING_TARGET_HOST);
        assert_eq!(body["status"], 400);
    }

    #[test]
    fn response_headers_are_set() {
        let response = OagwProblem::from(DomainError::RateLimitExceeded {
            retry_after_seconds: 7,
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(OAGW_ERROR_SOURCE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
        assert_eq!(response.headers().get("retry-after").unwrap(), "7");
        let content_type = response.headers().get("content-type").unwrap();
        assert!(
            content_type
                .to_str()
                .unwrap()
                .starts_with("application/problem+json")
        );
    }

    #[test]
    fn upstream_errors_are_labelled() {
        // ADR 0007: a synthesised problem document is gateway-owned, whatever it
        // describes; `upstream` is reserved for a passthrough response.
        let response = OagwProblem::from(DomainError::DownstreamError {
            detail: "boom".to_owned(),
            upstream_id: Some(uuid::Uuid::new_v4()),
            host: Some("api.openai.com".to_owned()),
            path: Some("/v1/chat".to_owned()),
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .headers()
                .get(OAGW_ERROR_SOURCE_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }

    #[test]
    fn target_host_errors_carry_the_valid_host_list() {
        let valid_hosts = vec!["us.vendor.com".to_owned(), "eu.vendor.com".to_owned()];
        let body = OagwProblem::from(DomainError::UnknownTargetHost {
            upstream_id: uuid::Uuid::new_v4(),
            invalid_value: "apac.vendor.com".to_owned(),
            valid_hosts: valid_hosts.clone(),
        })
        .to_json();
        assert_eq!(body["valid_hosts"], serde_json::json!(valid_hosts));

        let body = OagwProblem::from(DomainError::InvalidTargetHost {
            upstream_id: uuid::Uuid::new_v4(),
            invalid_value: "us.vendor.com:8443".to_owned(),
            valid_hosts,
        })
        .to_json();
        assert_eq!(
            body["valid_hosts"],
            serde_json::json!(["us.vendor.com", "eu.vendor.com"])
        );
    }

    #[test]
    fn duration_helpers() {
        assert_eq!(Duration::from_secs(3).as_secs(), 3);
    }
}
