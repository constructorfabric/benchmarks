//! Fake OAGW for the provider adapter and knowledge retriever tests: canned
//! replies (FIFO), captured requests, a flag set when a response body is
//! dropped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, ServiceGatewayClientV1,
    UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::infra::llm::ServiceIdentity;
use crate::infra::llm::providers::OagwLlmClient;
use crate::test_support::test_ctx;

/// A canned reply for the next `proxy_request`.
pub enum Reply {
    Response {
        status: u16,
        headers: Vec<(&'static str, String)>,
        source: Option<ErrorSource>,
        chunks: Vec<Bytes>,
        /// Keep the body open after the chunks (never ends by itself).
        hang: bool,
    },
    Error(CanonicalError),
}

#[derive(Debug, Clone)]
pub struct Captured {
    pub method: http::Method,
    pub uri: String,
    pub content_type: Option<String>,
    pub headers: http::HeaderMap,
    /// The JSON body (`Null` when the body is not JSON).
    pub body: Value,
    pub subject_id: Uuid,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

#[derive(Default)]
pub struct FakeGw {
    pub replies: Mutex<Vec<Reply>>,
    pub captured: Mutex<Vec<Captured>>,
    /// Set when the response body stream is dropped.
    pub body_dropped: Arc<AtomicBool>,
}

impl FakeGw {
    pub fn with(reply: Reply) -> Arc<Self> {
        let gw = Arc::new(Self::default());
        gw.push(reply);
        gw
    }

    pub fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push(reply);
    }

    pub fn last(&self) -> Captured {
        self.captured.lock().unwrap().last().cloned().unwrap()
    }

    pub fn dropped(&self) -> bool {
        self.body_dropped.load(Ordering::SeqCst)
    }
}

/// Flags `dropped` when the body stream is dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn unused() -> CanonicalError {
    CanonicalError::internal("not used by the llm client").create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGw {
    async fn create_upstream(
        &self,
        _: SecurityContext,
        _: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }
    async fn get_upstream(&self, _: SecurityContext, _: Uuid) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }
    async fn list_upstreams(
        &self,
        _: SecurityContext,
        _: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Err(unused())
    }
    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unused())
    }
    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Err(unused())
    }
    async fn create_route(
        &self,
        _: SecurityContext,
        _: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unused())
    }
    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        Err(unused())
    }
    async fn list_routes(
        &self,
        _: SecurityContext,
        _: Option<Uuid>,
        _: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Err(unused())
    }
    async fn update_route(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unused())
    }
    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        Err(unused())
    }
    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unused())
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let bytes = body.into_bytes().await.unwrap();
        self.captured.lock().unwrap().push(Captured {
            method: parts.method,
            uri: parts.uri.to_string(),
            content_type: parts
                .headers
                .get(http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap().to_owned()),
            headers: parts.headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            subject_id: ctx.subject_id(),
        });
        let reply = self.replies.lock().unwrap().remove(0);
        match reply {
            Reply::Error(e) => Err(e),
            Reply::Response {
                status,
                headers,
                source,
                chunks,
                hang,
            } => {
                let flag = DropFlag(Arc::clone(&self.body_dropped));
                let items = futures::stream::iter(chunks.into_iter().map(Ok));
                let tail = futures::stream::unfold((hang, flag), |(hang, flag)| async move {
                    if hang {
                        futures::future::pending::<()>().await;
                    }
                    drop(flag);
                    None::<(Result<Bytes, oagw_sdk::body::BoxError>, (bool, DropFlag))>
                });
                let stream: oagw_sdk::body::BodyStream = Box::pin(items.chain(tail));
                let mut builder = http::Response::builder().status(status);
                for (k, v) in headers {
                    builder = builder.header(k, v);
                }
                let mut resp = builder.body(Body::Stream(stream)).unwrap();
                if let Some(source) = source {
                    resp.extensions_mut().insert(source);
                }
                Ok(resp)
            }
        }
    }
}

// ── Reply builders ───────────────────────────────────────────────────────────

/// SSE frames with an `event:` line each.
pub fn sse(frames: &[(&str, Value)]) -> Reply {
    let mut body = String::new();
    for (event, data) in frames {
        write!(body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    sse_raw(&body)
}

/// SSE frames without `event:` lines, then `data: [DONE]` when `done`.
pub fn sse_data(frames: &[Value], done: bool) -> Reply {
    let mut body = String::new();
    for data in frames {
        write!(body, "data: {data}\n\n").unwrap();
    }
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    sse_raw(&body)
}

pub fn sse_raw(body: &str) -> Reply {
    Reply::Response {
        status: 200,
        headers: vec![("content-type", "text/event-stream".to_owned())],
        source: None,
        chunks: vec![Bytes::from(body.to_owned())],
        hang: false,
    }
}

/// A 200 JSON reply.
pub fn json_ok(body: &Value) -> Reply {
    Reply::Response {
        status: 200,
        headers: vec![("content-type", "application/json".to_owned())],
        source: None,
        chunks: vec![Bytes::from(body.to_string())],
        hang: false,
    }
}

pub fn http_error(
    status: u16,
    source: ErrorSource,
    headers: Vec<(&'static str, String)>,
    body: &Value,
) -> Reply {
    Reply::Response {
        status,
        headers,
        source: Some(source),
        chunks: vec![Bytes::from(body.to_string())],
        hang: false,
    }
}

/// An [`OagwLlmClient`] over `gw` with a ready service identity.
pub fn client(gw: &Arc<FakeGw>) -> (OagwLlmClient, SecurityContext) {
    let (gw, identity, ctx) = parts(gw);
    (OagwLlmClient::new(gw, identity), ctx)
}

/// The gateway and a ready service identity (and its context).
pub fn parts(
    gw: &Arc<FakeGw>,
) -> (
    Arc<dyn ServiceGatewayClientV1>,
    Arc<ServiceIdentity>,
    SecurityContext,
) {
    let identity = Arc::new(ServiceIdentity::default());
    let ctx = test_ctx();
    identity.set(ctx.clone());
    let gw: Arc<dyn ServiceGatewayClientV1> = gw.clone();
    (gw, identity, ctx)
}
