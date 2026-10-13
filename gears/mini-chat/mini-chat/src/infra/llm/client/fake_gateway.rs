//! Recording, scriptable fake of `ServiceGatewayClientV1` (tests only).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{
    AuthConfig, Body, CreateRouteRequest, CreateUpstreamRequest, HttpMatch, ListQuery, Route,
    ServiceGatewayClientV1, UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use parking_lot::Mutex;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::derive_alias;

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub(crate) struct FakeUpstreamError;

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
pub(crate) struct FakeProxyError;

/// Canonical errors used by tests.
pub(crate) mod errs {
    use super::{CanonicalError, FakeProxyError, FakeUpstreamError};

    pub(crate) fn already_exists() -> CanonicalError {
        FakeUpstreamError::already_exists("duplicate")
            .with_resource("dup")
            .create()
    }

    pub(crate) fn invalid_argument(detail: &str) -> CanonicalError {
        FakeUpstreamError::invalid_argument()
            .with_format(detail)
            .create()
    }

    pub(crate) fn failed_precondition() -> CanonicalError {
        FakeUpstreamError::failed_precondition()
            .with_precondition_violation("auth.config.secret_ref", "secret not readable", "STATE")
            .create()
    }

    pub(crate) fn deadline_exceeded() -> CanonicalError {
        FakeProxyError::deadline_exceeded("upstream timeout").create()
    }

    pub(crate) fn unavailable() -> CanonicalError {
        CanonicalError::service_unavailable()
            .with_detail("link down https://internal.example/x")
            .create()
    }
}

/// Recorded call.
#[derive(Debug, Clone)]
pub(crate) enum Call {
    CreateUpstream {
        alias: Option<String>,
        scheme: oagw_sdk::Scheme,
        host: String,
        port: u16,
        protocol: String,
        auth: Option<AuthConfig>,
    },
    ListUpstreams,
    CreateRoute {
        upstream_id: Uuid,
        http: HttpMatch,
    },
    Proxy {
        method: http::Method,
        uri: String,
        headers: http::HeaderMap,
        body: Bytes,
    },
    Other(#[allow(dead_code)] &'static str),
}

/// Response body of a scripted proxy response.
pub(crate) enum FakeBody {
    Bytes(Vec<u8>),
    /// Chunks streamed one by one, then the body ends.
    Chunks(Vec<String>),
    /// Chunks, then the body stays open forever; `dropped` is set when the body is dropped.
    Hang(Vec<String>, Arc<AtomicBool>),
}

/// Scripted proxy result.
pub(crate) enum ProxyScript {
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: FakeBody,
        source: Option<ErrorSource>,
    },
    Err(CanonicalError),
}

impl ProxyScript {
    pub(crate) fn sse(chunks: Vec<String>) -> Self {
        Self::Response {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: FakeBody::Chunks(chunks),
            source: None,
        }
    }

    pub(crate) fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::Response {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: FakeBody::Bytes(serde_json::to_vec(body).unwrap()),
            source: (status >= 400).then_some(ErrorSource::Upstream),
        }
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub(crate) struct FakeGateway {
    pub calls: Mutex<Vec<Call>>,
    pub proxy: Mutex<VecDeque<ProxyScript>>,
    /// Scripted `create_upstream` failures (`None` = succeed).
    pub upstream_results: Mutex<VecDeque<Option<CanonicalError>>>,
    /// Scripted `create_route` failures (`None` = succeed).
    pub route_results: Mutex<VecDeque<Option<CanonicalError>>>,
    pub upstreams: Mutex<Vec<Upstream>>,
}

pub(crate) fn upstream(alias: &str, host: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: Uuid::nil(),
        alias: alias.to_owned(),
        server: oagw_sdk::Server {
            endpoints: vec![oagw_sdk::Endpoint {
                scheme: oagw_sdk::Scheme::Https,
                host: host.to_owned(),
                port: 443,
            }],
        },
        protocol: oagw_sdk::HTTP_PROTOCOL_ID.to_owned(),
        enabled: true,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: vec![],
    }
}

impl FakeGateway {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn push(&self, s: ProxyScript) {
        self.proxy.lock().push_back(s);
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }

    pub(crate) fn proxy_calls(&self) -> Vec<(http::Method, String, http::HeaderMap, Bytes)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Proxy {
                    method,
                    uri,
                    headers,
                    body,
                } => Some((method, uri, headers, body)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn last_json_body(&self) -> serde_json::Value {
        let (_, _, _, body) = self.proxy_calls().pop().expect("no proxy call");
        serde_json::from_slice(&body).expect("json body")
    }

    pub(crate) fn upstream_calls(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::CreateUpstream { .. }))
            .collect()
    }

    pub(crate) fn route_calls(&self) -> Vec<(Uuid, HttpMatch)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::CreateRoute { upstream_id, http } => Some((upstream_id, http)),
                _ => None,
            })
            .collect()
    }
}

fn not_used() -> CanonicalError {
    CanonicalError::internal("not used by mini-chat").create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGateway {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let ep = req.server().endpoints[0].clone();
        self.calls.lock().push(Call::CreateUpstream {
            alias: req.alias().map(str::to_owned),
            scheme: ep.scheme,
            host: ep.host.clone(),
            port: ep.port,
            protocol: req.protocol().to_owned(),
            auth: req.auth().cloned(),
        });
        if let Some(Some(e)) = self.upstream_results.lock().pop_front() {
            return Err(e);
        }
        let alias = req
            .alias()
            .map_or_else(|| derive_alias(&ep.host, ep.port), str::to_owned);
        let mut u = upstream(&alias, &ep.host);
        u.server.endpoints[0] = ep;
        self.upstreams.lock().push(u.clone());
        Ok(u)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.calls.lock().push(Call::Other("get_upstream"));
        Err(not_used())
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        self.calls.lock().push(Call::ListUpstreams);
        Ok(self
            .upstreams
            .lock()
            .iter()
            .skip(query.skip as usize)
            .take(query.top as usize)
            .cloned()
            .collect())
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.calls.lock().push(Call::Other("update_upstream"));
        Err(not_used())
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        self.calls.lock().push(Call::Other("delete_upstream"));
        Err(not_used())
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let http = req.match_rules().http.clone().expect("http match");
        self.calls.lock().push(Call::CreateRoute {
            upstream_id: req.upstream_id(),
            http,
        });
        if let Some(Some(e)) = self.route_results.lock().pop_front() {
            return Err(e);
        }
        Ok(Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            priority: 0,
            enabled: true,
        })
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        self.calls.lock().push(Call::Other("get_route"));
        Err(not_used())
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        self.calls.lock().push(Call::Other("list_routes"));
        Ok(vec![])
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.calls.lock().push(Call::Other("update_route"));
        Err(not_used())
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        self.calls.lock().push(Call::Other("delete_route"));
        Err(not_used())
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        self.calls.lock().push(Call::Other("resolve_proxy_target"));
        Err(not_used())
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body = body.into_bytes().await.unwrap_or_default();
        self.calls.lock().push(Call::Proxy {
            method: parts.method.clone(),
            uri: parts.uri.to_string(),
            headers: parts.headers.clone(),
            body,
        });
        let script = self.proxy.lock().pop_front();
        let Some(script) = script else {
            return Ok(http::Response::builder()
                .status(500)
                .body(Body::from("no script"))
                .unwrap());
        };
        match script {
            ProxyScript::Err(e) => Err(e),
            ProxyScript::Response {
                status,
                headers,
                body,
                source,
            } => {
                let mut b = http::Response::builder().status(status);
                for (k, v) in headers {
                    b = b.header(k, v);
                }
                let body = match body {
                    FakeBody::Bytes(v) => Body::from(v),
                    FakeBody::Chunks(chunks) => Body::Stream(Box::pin(futures::stream::iter(
                        chunks
                            .into_iter()
                            .map(|c| Ok::<_, oagw_sdk::body::BoxError>(Bytes::from(c))),
                    ))),
                    FakeBody::Hang(chunks, flag) => {
                        let guard = DropFlag(flag);
                        Body::Stream(Box::pin(async_stream::stream! {
                            let _guard = guard;
                            for c in chunks {
                                yield Ok::<_, oagw_sdk::body::BoxError>(Bytes::from(c));
                            }
                            futures::future::pending::<()>().await;
                        }))
                    }
                };
                let mut resp = b.body(body).unwrap();
                if let Some(s) = source {
                    resp.extensions_mut().insert(s);
                }
                Ok(resp)
            }
        }
    }
}
