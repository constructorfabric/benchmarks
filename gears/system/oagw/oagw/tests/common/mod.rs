//! Shared test doubles and harness helpers for the OAGW integration tests
//! (`control_plane_rest.rs`, `data_plane.rs`).
//!
//! Provides an always-allowing PEP double (mirroring the real
//! static-authz-plugin: `decision=true` plus a tenant-scope constraint so the
//! PEP constraint compiler sees a supported property), SecurityContext
//! builders, and the management axum router with an allow-all PEP.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::pep::PolicyEnforcer;
use authz_resolver_sdk::{AuthZResolverClient, AuthZResolverError};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use toolkit_security::constants::{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID};
use toolkit_security::pep_properties;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::ProxyPort;
use oagw::api::rest::routes::register_routes;
use oagw::domain::repository::ControlPlaneService;

/// The platform system tenant used as the in-scope caller across tests.
pub const SYSTEM: (Uuid, Uuid) = (DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID);

/// Always-allowing PDP double that mirrors the real static-authz-plugin:
/// `decision=true` with a tenant-scope constraint on the subject's tenant
/// (so the PEP constraint compiler accepts the response).
pub struct AllowAllPdp;

#[async_trait]
impl AuthZResolverClient for AllowAllPdp {
    async fn evaluate(
        &self,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        let tid = req
            .subject
            .properties
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or(DEFAULT_TENANT_ID);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [tid],
                    ))],
                }],
                ..Default::default()
            },
        })
    }
}

/// Always-denying PDP double (`decision=false` with a deny reason), mirroring
/// the real PEP deny path the management and proxy gates surface as 403.
pub struct DenyPdp;

#[async_trait]
impl AuthZResolverClient for DenyPdp {
    async fn evaluate(
        &self,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                deny_reason: Some(authz_resolver_sdk::models::DenyReason {
                    error_code: "permission_denied".to_owned(),
                    details: Some("test PDP denies every access".to_owned()),
                }),
                ..Default::default()
            },
        })
    }
}

/// An always-allowing PEP enforcer over [`AllowAllPdp`].
#[must_use]
pub fn enforcer() -> Arc<PolicyEnforcer> {
    Arc::new(PolicyEnforcer::new(Arc::new(AllowAllPdp)))
}

/// An always-denying PEP enforcer over [`DenyPdp`].
#[must_use]
pub fn deny_enforcer() -> Arc<PolicyEnforcer> {
    Arc::new(PolicyEnforcer::new(Arc::new(DenyPdp)))
}

/// Builds a caller security context.
#[must_use]
pub fn security(subject: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("valid test security context")
}

/// The management router backing [`router`]: control plane + given PEP + port
/// cell + caller identity.
pub fn router_with(
    control: Arc<ControlPlaneService>,
    identity: SecurityContext,
    enforcer: Arc<PolicyEnforcer>,
) -> Router {
    let openapi: Arc<OpenApiRegistryImpl> = Arc::new(OpenApiRegistryImpl::new());
    let port = ProxyPort::new();
    register_routes(Router::new(), &*openapi, control, enforcer, port)
        .layer(axum::Extension(identity))
}

/// The management router: control plane + allow-all PEP + port cell + caller
/// identity.
///
/// The `ProxyPort` cell is left unbound (`port = 0`), which the proxy relay
/// handler reports as "data plane not ready" (503).
pub fn router(control: Arc<ControlPlaneService>, identity: SecurityContext) -> Router {
    router_with(control, identity, enforcer())
}

/// Runs one request against the router on a fresh runtime.
pub fn run(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let builder = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json");
            let request = match body {
                Some(b) => builder.body(Body::from(b.to_string())).unwrap(),
                None => builder.body(Body::empty()).unwrap(),
            };
            let resp = app.oneshot(request).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            (status, bytes)
        })
}

/// Parses a response body as JSON (`Null` for an empty body).
pub fn json_status(resp: (StatusCode, Vec<u8>)) -> (StatusCode, serde_json::Value) {
    match resp.1.is_empty() {
        true => (resp.0, serde_json::Value::Null),
        false => (resp.0, serde_json::from_slice(&resp.1).expect("json body")),
    }
}
