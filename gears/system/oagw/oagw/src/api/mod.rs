//! The gear's HTTP API: management handlers, the proxy handlers and the router.

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::api::problem;

pub mod dto;
pub mod management;
pub mod proxy;
pub mod ws;

use self::ws::proxy_ws;

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;

/// Tenant attributed to a request that arrives without a `SecurityContext`.
pub const PUBLIC_TENANT: Uuid = Uuid::nil();

/// Resolves the ancestor chain a caller's tenant sits in (DESIGN §4.3).
#[async_trait::async_trait]
pub trait AncestorSource: Send + Sync {
    /// The chain from the caller's tenant outward, excluding the tenant itself.
    async fn ancestors(&self, tenant: Uuid) -> Vec<Uuid>;
}

/// Source that always reports an empty chain; used by tests and stand-alone deployments.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAncestors;

#[async_trait::async_trait]
impl AncestorSource for NoAncestors {
    async fn ancestors(&self, _tenant: Uuid) -> Vec<Uuid> {
        Vec::new()
    }
}

/// Shared state of the gear's API.
#[derive(Clone)]
pub struct ApiState {
    /// Control plane.
    pub control: Arc<crate::domain::ControlPlane>,
    /// Data plane.
    pub data: Arc<crate::infra::proxy::service::DataPlane>,
    /// Tenant ancestor resolution.
    pub ancestors: Arc<dyn AncestorSource>,
}

impl ApiState {
    /// State over the given planes.
    #[must_use]
    pub fn new(
        control: Arc<crate::domain::ControlPlane>,
        data: Arc<crate::infra::proxy::service::DataPlane>,
        ancestors: Arc<dyn AncestorSource>,
    ) -> Self {
        Self {
            control,
            data,
            ancestors,
        }
    }
}

/// The tenant of the caller, falling back to the anonymous tenant when absent.
///
/// The platform api-gateway inserts the `SecurityContext`; a request that reaches the gear
/// without one is treated as anonymous.
#[must_use]
pub fn tenant_of(request: &Request) -> Uuid {
    request
        .extensions()
        .get::<SecurityContext>()
        .map_or(PUBLIC_TENANT, |ctx| {
            ctx.subject_tenant_id()
        })
}

/// JSON body extractor that renders OAGW problem details on rejection.
///
/// `axum::Json` would answer with its own shape and status; every gateway error has to be an
/// RFC 9457 problem (DESIGN §Error Response Format).
pub struct JsonBody<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(
        request: Request,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(request, _state)
            .await
            .map_err(|error| reject("unreadable-request-body", &error.to_string()))?;
        if bytes.is_empty() {
            return Err(reject("invalid-payload", "a request body is required"));
        }
        serde_json::from_slice(&bytes)
            .map(JsonBody)
            .map_err(|error| reject("invalid-payload", &error.to_string()))
    }
}

fn reject(code: &str, detail: &str) -> Response {
    let error = DomainError::Validation(format!("{code}: {detail}"));
    let meta = crate::domain::ProblemMeta::new()
        .with_code(code)
        .with_invalid_value(detail);
    problem::problem_response(&error, &meta, "/oagw/v1")
}

/// Mount every route of the gear on `router`.
#[must_use = "the router has to be served to take effect"]
pub fn router(state: ApiState) -> Router {
    let management = management::routes();
    let proxy = proxy::routes();
    management
        .merge(proxy)
        .layer(axum::Extension(state))
}

/// Uniform JSON success response.
pub fn json_response<T: serde::Serialize>(status: axum::http::StatusCode, body: &T) -> Response {
    (
        status,
        axum::Json(serde_json::to_value(body).unwrap_or(serde_json::Value::Null)),
    )
        .into_response()
}

#[cfg(test)]
#[path = "json_body_tests.rs"]
mod json_body_tests;
