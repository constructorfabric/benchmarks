//! Pattern: Custom transport trait via octocrab
//!
//! Implements `tower::Service` on a `GatewayService` adapter that routes
//! octocrab's HTTP requests through `ServiceGatewayClientV1::proxy_request`.
//! This demonstrates the idiomatic Rust pattern for pluggable HTTP transports.
//!
//! Note: authentication with the upstream API (e.g. GitHub PAT) is configured
//! on the OAGW upstream and injected transparently by the gateway — the
//! octocrab client itself needs no auth configuration.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// Canned GitHub API response (repository object)
// ---------------------------------------------------------------------------

const CANNED_REPO_RESPONSE: &str = r#"{
    "id": 42,
    "node_id": "MDEwOlJlcG9zaXRvcnk0Mg==",
    "name": "example-repo",
    "full_name": "test-owner/example-repo",
    "private": false,
    "owner": {
        "login": "test-owner",
        "id": 1,
        "node_id": "MDQ6VXNlcjE=",
        "avatar_url": "https://avatars.githubusercontent.com/u/1",
        "gravatar_id": "",
        "url": "https://api.github.com/users/test-owner",
        "html_url": "https://github.com/test-owner",
        "followers_url": "https://api.github.com/users/test-owner/followers",
        "following_url": "https://api.github.com/users/test-owner/following{/other_user}",
        "gists_url": "https://api.github.com/users/test-owner/gists{/gist_id}",
        "starred_url": "https://api.github.com/users/test-owner/starred{/owner}{/repo}",
        "subscriptions_url": "https://api.github.com/users/test-owner/subscriptions",
        "organizations_url": "https://api.github.com/users/test-owner/orgs",
        "repos_url": "https://api.github.com/users/test-owner/repos",
        "events_url": "https://api.github.com/users/test-owner/events{/privacy}",
        "received_events_url": "https://api.github.com/users/test-owner/received_events",
        "type": "User",
        "site_admin": false
    },
    "html_url": "https://github.com/test-owner/example-repo",
    "description": "A test repository routed through OAGW",
    "fork": false,
    "url": "https://api.github.com/repos/test-owner/example-repo",
    "created_at": "2024-01-01T00:00:00Z",
    "updated_at": "2024-06-01T00:00:00Z",
    "pushed_at": "2024-06-01T00:00:00Z",
    "default_branch": "main"
}"#;

fn canned_response() -> http::Response<Body> {
    http::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(CANNED_REPO_RESPONSE))
        .unwrap()
}

// ---------------------------------------------------------------------------
// MockGateway — same pattern as usage.rs
// ---------------------------------------------------------------------------

struct MockGateway {
    response: Mutex<Option<http::Response<Body>>>,
}

impl MockGateway {
    fn responding_with(resp: http::Response<Body>) -> Self {
        Self {
            response: Mutex::new(Some(resp)),
        }
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for MockGateway {
    async fn create_upstream(
        &self,
        _: SecurityContext,
        _: oagw_sdk::CreateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        unimplemented!()
    }
    async fn get_upstream(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        unimplemented!()
    }
    async fn list_upstreams(
        &self,
        _: SecurityContext,
        _: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Upstream>, CanonicalError> {
        unimplemented!()
    }
    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
        _: oagw_sdk::UpdateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        unimplemented!()
    }
    async fn delete_upstream(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!()
    }
    async fn create_route(
        &self,
        _: SecurityContext,
        _: oagw_sdk::CreateRouteRequest,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        unimplemented!()
    }
    async fn get_route(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        unimplemented!()
    }
    async fn list_routes(
        &self,
        _: SecurityContext,
        _: Option<uuid::Uuid>,
        _: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Route>, CanonicalError> {
        unimplemented!()
    }
    async fn update_route(
        &self,
        _: SecurityContext,
        _: uuid::Uuid,
        _: oagw_sdk::UpdateRouteRequest,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        unimplemented!()
    }
    async fn delete_route(&self, _: SecurityContext, _: uuid::Uuid) -> Result<(), CanonicalError> {
        unimplemented!()
    }
    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(oagw_sdk::Upstream, oagw_sdk::Route), CanonicalError> {
        unimplemented!()
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        _req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        Ok(self
            .response
            .lock()
            .unwrap()
            .take()
            .expect("response already consumed"))
    }
}

// ---------------------------------------------------------------------------
// GatewayService — tower::Service adapter for proxy_request
// ---------------------------------------------------------------------------

struct GatewayService<G> {
    gateway: Arc<G>,
    ctx: SecurityContext,
}

impl<G> Clone for GatewayService<G> {
    fn clone(&self) -> Self {
        Self {
            gateway: Arc::clone(&self.gateway),
            ctx: self.ctx.clone(),
        }
    }
}

impl<G, B> tower::Service<Request<B>> for GatewayService<G>
where
    G: ServiceGatewayClientV1 + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    type Response = Response<Full<Bytes>>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let gateway = Arc::clone(&self.gateway);
        let ctx = self.ctx.clone();

        Box::pin(async move {
            // Convert any http_body::Body → oagw_sdk::Body
            let (parts, body) = req.into_parts();
            let collected = BodyExt::collect(body).await.map_err(Into::into)?;
            let body_bytes = collected.to_bytes();
            let oagw_body = if body_bytes.is_empty() {
                Body::Empty
            } else {
                Body::from(body_bytes)
            };
            let oagw_req = Request::from_parts(parts, oagw_body);

            // Route through the gateway
            let oagw_resp = gateway.proxy_request(ctx, oagw_req).await?;

            // Convert oagw_sdk::Body → Full<Bytes>
            let (parts, body) = oagw_resp.into_parts();
            let resp_bytes = body.into_bytes().await?;
            Ok(Response::from_parts(parts, Full::new(resp_bytes)))
        })
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn octocrab_custom_transport() -> TestResult {
    // -- setup: gateway returning a canned GitHub repo response -----------------
    let gateway = MockGateway::responding_with(canned_response());
    let service = GatewayService {
        gateway: Arc::new(gateway),
        ctx: SecurityContext::anonymous(),
    };

    // Build octocrab with our custom transport (no HTTP client needed)
    let octocrab = octocrab::OctocrabBuilder::new_empty()
        .with_service(service)
        .with_auth(octocrab::AuthState::None)
        .build()
        .expect("infallible");

    // -- action: use octocrab's typed API to fetch a repo -----------------------
    let repo: octocrab::models::Repository =
        octocrab.repos("test-owner", "example-repo").get().await?;

    // -- verify: response deserialized into octocrab types ----------------------
    assert_eq!(repo.name, "example-repo");
    assert_eq!(repo.full_name.as_deref(), Some("test-owner/example-repo"));
    assert_eq!(
        repo.description.as_deref(),
        Some("A test repository routed through OAGW")
    );

    Ok(())
}
