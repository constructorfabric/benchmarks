//! Test fakes shared by the adapter and provisioning tests: an in-memory
//! [`ServiceGatewayClientV1`] and a fixed S2S context source.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Response};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::{Body, BoxError};
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, ListQuery, MatchRules, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::infra::oagw_provisioning::{SystemContextError, SystemContextSource};

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub struct UpstreamScope;

#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
pub struct RouteScope;

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
pub struct ProxyScope;

pub fn upstream_exists(alias: &str) -> CanonicalError {
    UpstreamScope::already_exists("alias already exists")
        .with_resource(alias)
        .create()
}

pub fn route_exists() -> CanonicalError {
    RouteScope::already_exists("route overlap")
        .with_resource("route")
        .create()
}

pub fn secret_not_accessible() -> CanonicalError {
    UpstreamScope::failed_precondition()
        .with_precondition_violation(
            "auth.config.secret_ref",
            "secret_ref 'cred://openai-key' is not accessible to this tenant",
            "STATE",
        )
        .create()
}

pub fn validation(detail: &str) -> CanonicalError {
    UpstreamScope::invalid_argument()
        .with_field_violation("upstream", detail, "VALIDATION")
        .create()
}

pub fn deadline_exceeded() -> CanonicalError {
    ProxyScope::deadline_exceeded("request timed out").create()
}

pub fn system_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x1111))
        .subject_tenant_id(Uuid::from_u128(0x2222))
        .build()
        .unwrap()
}

/// Fixed S2S context source; `fail_with` makes it fail instead.
#[derive(Default)]
pub struct StaticSystemContext {
    pub fail_with: Mutex<Option<SystemContextError>>,
}

#[async_trait]
impl SystemContextSource for StaticSystemContext {
    async fn system_context(&self) -> Result<SecurityContext, SystemContextError> {
        match lock(&self.fail_with).clone() {
            Some(err) => Err(err),
            None => Ok(system_ctx()),
        }
    }
}

/// One proxied request.
#[derive(Debug, Clone)]
pub struct Proxied {
    pub method: http::Method,
    pub uri: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
    /// The request body as sent (multipart uploads are not JSON).
    pub raw: Bytes,
    pub tenant_id: Uuid,
}

/// In-memory gateway: upstreams and routes with OAGW's conflict rules, and
/// scripted proxy responses.
#[derive(Default)]
pub struct FakeGateway {
    pub proxy_responses: Mutex<VecDeque<Result<Response<Body>, CanonicalError>>>,
    pub proxied: Mutex<Vec<Proxied>>,
    pub upstreams: Mutex<Vec<Upstream>>,
    pub routes: Mutex<Vec<Route>>,
    pub create_upstream_errors: Mutex<VecDeque<CanonicalError>>,
    pub create_route_errors: Mutex<VecDeque<CanonicalError>>,
    /// Route paths whose creation fails with a validation error.
    pub failing_route_paths: Mutex<Vec<String>>,
}

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// SSE response with the given body chunks.
pub fn sse_response(chunks: Vec<String>) -> Response<Body> {
    let items: Vec<Result<Bytes, BoxError>> =
        chunks.into_iter().map(|c| Ok(Bytes::from(c))).collect();
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::Stream(Box::pin(futures::stream::iter(items))))
        .unwrap()
}

/// JSON response with the given status and optional error source.
pub fn json_response(
    status: u16,
    body: &serde_json::Value,
    headers: &[(&str, &str)],
    source: Option<ErrorSource>,
) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut response = builder
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    if let Some(source) = source {
        response.extensions_mut().insert(source);
    }
    response
}

impl FakeGateway {
    pub fn push_proxy(&self, response: Result<Response<Body>, CanonicalError>) {
        lock(&self.proxy_responses).push_back(response);
    }

    pub fn routes_of(&self, upstream_id: Uuid) -> Vec<MatchRules> {
        lock(&self.routes)
            .iter()
            .filter(|route| route.upstream_id == upstream_id)
            .map(|route| route.match_rules.clone())
            .collect()
    }
}

fn unused(method: &str) -> CanonicalError {
    CanonicalError::internal(format!("fake gateway: {method} is not used")).create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGateway {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        if let Some(err) = lock(&self.create_upstream_errors).pop_front() {
            return Err(err);
        }
        let alias = req.alias().unwrap_or_default().to_ascii_lowercase();
        let mut upstreams = lock(&self.upstreams);
        if upstreams.iter().any(|upstream| upstream.alias == alias) {
            return Err(upstream_exists(&alias));
        }
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            alias,
            server: req.server().clone(),
            protocol: req.protocol().to_owned(),
            enabled: req.enabled(),
            auth: req.auth().cloned(),
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: req.tags().to_vec(),
        };
        upstreams.push(upstream.clone());
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused("get_upstream"))
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        let skip = usize::try_from(query.skip).unwrap();
        let top = usize::try_from(query.top).unwrap();
        Ok(lock(&self.upstreams)
            .iter()
            .skip(skip)
            .take(top)
            .cloned()
            .collect())
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused("update_upstream"))
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        Err(unused("delete_upstream"))
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        if let Some(err) = lock(&self.create_route_errors).pop_front() {
            return Err(err);
        }
        let candidate = req.match_rules().http.clone().unwrap();
        if lock(&self.failing_route_paths).contains(&candidate.path) {
            return Err(validation("route rejected"));
        }
        let mut routes = lock(&self.routes);
        let overlaps = routes.iter().any(|route| {
            let existing = route.match_rules.http.as_ref().unwrap();
            route.upstream_id == req.upstream_id()
                && existing.path == candidate.path
                && existing
                    .methods
                    .iter()
                    .any(|m| candidate.methods.contains(m))
        });
        if overlaps {
            return Err(route_exists());
        }
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: Vec::new(),
            priority: req.priority(),
            enabled: req.enabled(),
        };
        routes.push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        Err(unused("get_route"))
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Err(unused("list_routes"))
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unused("update_route"))
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Err(unused("delete_route"))
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unused("resolve_proxy_target"))
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap();
        lock(&self.proxied).push(Proxied {
            method: parts.method,
            uri: parts.uri.to_string(),
            headers: parts.headers,
            body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            raw: bytes.clone(),
            tenant_id: ctx.subject_tenant_id(),
        });
        lock(&self.proxy_responses)
            .pop_front()
            .unwrap_or_else(|| Err(unused("proxy_request without a scripted response")))
    }
}
