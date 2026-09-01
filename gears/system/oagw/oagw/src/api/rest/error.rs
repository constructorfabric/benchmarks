//! REST error mapping: every [`DomainError`] becomes an RFC 9457
//! `application/problem+json` response carrying the exact GTS `type`
//! identifier of the `DESIGN.md` §3.3 contract table plus the two ADR-0004
//! CORS types, and `X-OAGW-Error-Source: gateway`.
//!
//! The REST layer also owns converting extractors ([`ValidJson`],
//! [`ValidQuery`], [`ValidPath`]) so that a malformed body, query string or
//! path parameter is reported as a canonical problem document instead of
//! axum's built-in `text/plain` rejection (`DESIGN.md` §3.3 "Error Response
//! Format").

use axum::extract::{FromRequest, FromRequestParts, OriginalUri, Request};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::domain::error::DomainError;

/// Response header marking gateway-originated errors.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Media type of every gateway-originated problem document (`DESIGN.md` §3.3,
/// ADR-0007).
pub const PROBLEM_MEDIA_TYPE: &str = "application/problem+json";

/// `X-OAGW-Error-Source` value for gateway-originated problems.
const ERROR_SOURCE: HeaderValue = HeaderValue::from_static("gateway");

/// `Content-Type` value of every gateway-originated problem.
const PROBLEM_CONTENT_TYPE: HeaderValue = HeaderValue::from_static(PROBLEM_MEDIA_TYPE);

/// The wire shape of a gateway-originated error.
///
/// The domain error is boxed: [`DomainError`] is large enough that carrying it
/// inline pushes every handler's `Result` past clippy's
/// `result_large_err` threshold, and the problem is only ever built once per
/// failing request.
#[derive(Debug, Clone)]
pub struct OagwProblem {
    error: Box<DomainError>,
    /// RFC 9457 `instance`: the request URI path, when the REST layer knows it.
    instance: Option<String>,
    /// `path` extension field: the request path the problem refers to.
    path: Option<String>,
    /// `host` extension field: the upstream alias of a data-plane request.
    host: Option<String>,
    /// HTTP status overriding the one derived from the error descriptor.
    status: Option<u16>,
}

impl OagwProblem {
    /// Wraps a domain error.
    #[must_use]
    pub fn new(error: DomainError) -> Self {
        Self {
            error: Box::new(error),
            instance: None,
            path: None,
            host: None,
            status: None,
        }
    }

    /// Wraps a domain error, pointing `instance` at the failing request path.
    #[must_use]
    pub fn at(path: &str, error: DomainError) -> Self {
        Self {
            instance: Some(path.to_owned()),
            ..Self::new(error)
        }
    }

    /// Wraps a data-plane domain error, adding the upstream `host` (alias) and
    /// the upstream-relative request `path`.
    #[must_use]
    pub fn for_upstream(alias: &str, request_path: &str, error: DomainError) -> Self {
        Self {
            host: Some(alias.to_owned()),
            path: Some(request_path.to_owned()),
            ..Self::new(error)
        }
    }

    /// Points the problem `instance` at `path`, overriding any earlier value.
    #[must_use]
    pub fn instance(mut self, path: String) -> Self {
        self.instance = Some(path);
        self
    }

    /// Overrides the HTTP status and the problem `status` field.
    ///
    /// Used by the converting extractors, which report axum's own rejection
    /// status (`415` for an unsupported media type, `422` for a body that does
    /// not match the declared schema) while keeping the canonical
    /// `cf.oagw.validation.error.v1` type.
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// The wrapped domain error.
    #[must_use]
    pub fn error(&self) -> &DomainError {
        &self.error
    }

    /// The problem document as a JSON value.
    #[must_use]
    pub fn body(&self) -> Value {
        let descriptor = self.error.descriptor();
        let status = self.status.unwrap_or(descriptor.status);
        let mut problem = Map::new();
        problem.insert("type".to_owned(), json!(descriptor.type_id));
        problem.insert("title".to_owned(), json!(descriptor.title));
        problem.insert("status".to_owned(), json!(status));
        problem.insert("detail".to_owned(), json!(self.error.detail()));
        if let Some(instance) = self.instance.as_ref() {
            problem.insert("instance".to_owned(), json!(instance));
        }
        problem.insert("context".to_owned(), context_of(&self.error));
        problem.insert("error_code".to_owned(), json!(descriptor.error_code));
        problem.insert("error_domain".to_owned(), json!("oagw.v1"));
        if let Some(seconds) = descriptor.retry_after_seconds {
            problem.insert("retry_after_seconds".to_owned(), json!(seconds));
        }
        if let DomainError::PluginInUse {
            plugin_id,
            referenced_by,
            ..
        } = self.error.as_ref()
        {
            problem.insert("plugin_id".to_owned(), json!(plugin_id));
            problem.insert("referenced_by".to_owned(), json!(referenced_by));
        }
        if let Some(host) = self.host.as_ref() {
            problem.insert("host".to_owned(), json!(host));
        }
        let path = self.path.as_ref().or(self.instance.as_ref()).cloned();
        if let Some(path) = path {
            problem.insert("path".to_owned(), json!(path));
        }
        Value::Object(problem)
    }

    /// The HTTP status of the response this problem renders to.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.status.unwrap_or_else(|| self.error.status()))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl From<DomainError> for OagwProblem {
    fn from(error: DomainError) -> Self {
        Self::new(error)
    }
}

impl std::fmt::Display for OagwProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.error)
    }
}

impl std::error::Error for OagwProblem {}

impl IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, axum::Json(self.body())).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE);
        headers.insert(HeaderName::from_static(ERROR_SOURCE_HEADER), ERROR_SOURCE);
        if let Some(seconds) = self.error.descriptor().retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response
                .headers_mut()
                .insert(HeaderName::from_static("retry-after"), value);
        }
        response
    }
}

/// Machine-readable `context` payload of a problem.
fn context_of(error: &DomainError) -> Value {
    if let DomainError::PluginInUse {
        plugin_id,
        referenced_by,
        ..
    } = error
    {
        return json!({
            "plugin_id": plugin_id,
            "referenced_by": referenced_by,
        });
    }
    json!({})
}

/// Renders a domain error as a JSON body (used by tests).
#[must_use]
pub fn problem_body(error: &DomainError) -> Value {
    OagwProblem::new(error.clone()).body()
}

/// The request URI path of the current request.
///
/// axum records the pre-nesting URI in the [`OriginalUri`] extension, so the
/// `instance` field carries the path the client called — including the `/api`
/// prefix of the nested mount — rather than the stripped one.
fn request_path(uri: &Uri, extensions: &Extensions) -> Option<String> {
    let path = extensions
        .get::<OriginalUri>()
        .map_or_else(|| uri.path(), |original| original.0.path());
    (!path.is_empty()).then(|| path.to_owned())
}

/// Builds the canonical problem for a rejected extractor.
fn rejection_problem(
    instance: Option<String>,
    rejection_status: StatusCode,
    detail: String,
) -> OagwProblem {
    OagwProblem {
        instance,
        status: Some(rejection_status.as_u16()),
        ..OagwProblem::new(DomainError::Validation(detail))
    }
}

/// A JSON body extractor that reports malformed bodies as canonical problems.
///
/// axum's [`Json`] rejection is `text/plain` with no GTS `type`, no
/// `error_code` and no `X-OAGW-Error-Source: gateway`; this wrapper maps it
/// onto [`DomainError::Validation`] while keeping axum's rejection status
/// (`400` for invalid JSON, `415` for a missing JSON content type, `422` for a
/// body that parses but does not match the declared schema).
#[derive(Debug, Clone)]
pub struct ValidJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = OagwProblem;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let instance = request_path(req.uri(), req.extensions());
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_problem(
                instance,
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// A query-string extractor that reports an unusable query as a problem.
///
/// axum's only query rejection is `400` (`FailedToDeserializeQueryString`), so
/// an unparsable or wrongly-typed query string is reported as the canonical
/// `400` validation problem.
#[derive(Debug, Clone)]
pub struct ValidQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ValidQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = OagwProblem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let instance = request_path(&parts.uri, &parts.extensions);
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_problem(
                instance,
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// A path extractor that reports unusable path parameters as a problem.
#[derive(Debug, Clone)]
pub struct ValidPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ValidPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = OagwProblem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let instance = request_path(&parts.uri, &parts.extensions);
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection_problem(
                instance,
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// The request URI path carried into the problem `instance` field.
///
/// Extracted by the data-plane handlers, which relay the raw request and
/// therefore need the original path of an upgrade or relayed call.
#[derive(Debug, Clone)]
pub struct RequestPath(pub String);

impl<S> FromRequestParts<S> for RequestPath
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            request_path(&parts.uri, &parts.extensions).unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
