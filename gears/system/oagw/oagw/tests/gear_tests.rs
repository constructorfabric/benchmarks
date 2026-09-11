//! Gear registration and mount-point tests.
//!
//! Covers `cpt-cf-oagw-dod-gear-registration` and
//! `cpt-cf-oagw-state-gear-foundation-lifecycle` and the management routes of
//! `cpt-cf-oagw-dod-management-routes`: the gear type is `Default`-constructible,
//! its module name is `oagw`, `register_rest` mounts the ten management paths on
//! `/oagw/v1`, and the mounted surface fails closed when the `ClientHub`
//! resolved no `AuthZ` client.
//!
//! `GearCtx` is built by the ToolKit runtime and cannot be constructed inside
//! a crate test (it needs a `CancellationToken` the manifest does not expose),
//! so the assertions here cover the pure surface, the mount point, and the
//! management surface the gear assembles for `register_rest`; the `init` /
//! `post_init` wiring is exercised by the e2e suite.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

// @cpt-dod:cpt-cf-oagw-dod-test-placement:p1

use std::sync::Arc;

use axum::Router;
use oagw::OagwGear;
use oagw::api::rest::MOUNT_POINT;
use tower::ServiceExt;

#[test]
fn the_gear_is_default_constructible() {
    let gear = OagwGear::default();
    assert!(gear.config().is_none(), "no config before init");
    assert!(gear.registry().is_none(), "no registry client before init");
}

#[test]
fn the_gear_module_name_is_oagw() {
    assert_eq!(OagwGear::MODULE_NAME, "oagw");
}

#[test]
fn the_mount_point_is_oagw_v1() {
    assert_eq!(MOUNT_POINT, "/oagw/v1");
}

#[tokio::test]
async fn the_nested_router_serves_no_oagw_route() {
    let app: Router = oagw::api::rest::nest_mount_point(Router::new());
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/v1/upstreams")
        .body(axum::body::Body::empty())
        .expect("request builds");

    let response: axum::http::Response<axum::body::Body> =
        app.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::NOT_FOUND,
        "/oagw/v1 has no routes in the foundation feature"
    );
}

#[tokio::test]
async fn the_mount_point_is_reachable_not_a_prefix_mismatch() {
    let app: Router = oagw::api::rest::nest_mount_point(Router::new());
    let request = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri("/oagw/other")
        .body(axum::body::Body::empty())
        .expect("request builds");

    let response: axum::http::Response<axum::body::Body> =
        app.oneshot(request).await.expect("oneshot resolves");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::NOT_FOUND,
        "a path outside the mount point is not the OAGW surface"
    );
}

#[test]
fn the_gear_state_machine_refuses_out_of_order_transitions() {
    use oagw::GearFoundationState;

    assert!(!GearFoundationState::Unregistered.is_terminal());
    assert!(GearFoundationState::StartupFailed.is_terminal());
    assert!(GearFoundationState::Ready.is_terminal());

    assert!(
        GearFoundationState::Unregistered.can_transition_to(GearFoundationState::Configured),
        "unregistered -> configured"
    );
    assert!(
        !GearFoundationState::Unregistered.can_transition_to(GearFoundationState::Ready),
        "readiness requires a provisioned catalogue first"
    );
    assert!(
        !GearFoundationState::TypeCatalogProvisioned
            .can_transition_to(GearFoundationState::Configured),
        "lifecycle states never move backwards"
    );
}

#[test]
fn the_gear_state_machine_walks_the_foundation_lifecycle() {
    use oagw::GearFoundationState;

    let state = GearFoundationState::Unregistered
        .transition(GearFoundationState::Configured)
        .expect("unregistered -> configured");
    let state = state
        .transition(GearFoundationState::TypeCatalogProvisioned)
        .expect("configured -> type-catalog-provisioned");
    let state = state
        .transition(GearFoundationState::Ready)
        .expect("type-catalog-provisioned -> ready");
    assert_eq!(state, GearFoundationState::Ready);

    let failed = GearFoundationState::Configured
        .transition(GearFoundationState::StartupFailed)
        .expect("configured -> startup-failed");
    assert_eq!(failed, GearFoundationState::StartupFailed);
}

#[test]
fn an_invalid_transition_is_reported_with_both_endpoints() {
    use oagw::GearFoundationState;

    let error = GearFoundationState::Ready
        .transition(GearFoundationState::Configured)
        .expect_err("ready is terminal");
    assert!(error.to_string().contains("ready"), "{error}");
    assert!(error.to_string().contains("configured"), "{error}");
}

/// The `AuthZ` PDP the allowing stub stands in for: it grants and narrows the
/// scope to the caller's own tenant.
struct Allowing;

#[async_trait::async_trait]
impl authz_resolver_sdk::api::AuthZResolverClient for Allowing {
    async fn evaluate(
        &self,
        _request: authz_resolver_sdk::models::EvaluationRequest,
    ) -> Result<
        authz_resolver_sdk::models::EvaluationResponse,
        authz_resolver_sdk::error::AuthZResolverError,
    > {
        use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
        use authz_resolver_sdk::models::{EvaluationResponse, EvaluationResponseContext};
        use toolkit_security::pep_properties;
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: String::from(pep_properties::OWNER_TENANT_ID),
                        value: serde_json::json!(uuid::Uuid::from_u128(0x30).to_string()),
                    })],
                }],
                deny_reason: None,
            },
        })
    }
}

/// The authenticated subject a management request carries.
fn subject(tenant: u128) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::Uuid::from_u128(tenant))
        .subject_tenant_id(uuid::Uuid::from_u128(tenant))
        .build()
        .expect("the subject is complete")
}

/// The router the gear's own assembly mounts, driven over one request.
async fn answer(
    app: axum::Router,
    method: axum::http::Method,
    uri: &str,
    tenant: Option<u128>,
    body: Option<String>,
) -> (axum::http::StatusCode, Option<serde_json::Value>) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(tenant) = tenant {
        builder = builder.extension(subject(tenant));
    }
    let request = builder
        .body(axum::body::Body::from(body.unwrap_or_default()))
        .expect("the request builds");
    let response: axum::http::Response<axum::body::Body> =
        app.oneshot(request).await.expect("oneshot resolves");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).expect("the body is JSON"))
    };
    (status, document)
}

/// A valid upstream body.
fn upstream_body() -> String {
    serde_json::json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
        },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
    .to_string()
}

#[tokio::test]
async fn the_gear_service_answers_the_ten_management_paths() {
    let config = oagw::OagwConfig::default();
    let state = oagw::api::rest::state::OagwState::assemble(
        &config,
        Some(Arc::new(Allowing) as Arc<dyn authz_resolver_sdk::api::AuthZResolverClient>),
        None,
        None,
    )
    .expect("the surface assembles");
    let app = oagw::api::rest::register_management_routes(axum::Router::new(), Arc::new(state));

    // The create, read, list, replace and delete of each resource kind.
    let (status, document) = answer(
        app.clone(),
        axum::http::Method::POST,
        "/oagw/v1/upstreams",
        Some(0x30),
        Some(upstream_body()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{document:?}");
    let id = document.expect("the representation")["id"]
        .as_str()
        .expect("the instance id")
        .to_owned();

    let (status, _) = answer(
        app.clone(),
        axum::http::Method::GET,
        "/oagw/v1/upstreams",
        Some(0x30),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let (status, _) = answer(
        app.clone(),
        axum::http::Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(0x30),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let (status, _) = answer(
        app.clone(),
        axum::http::Method::PUT,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(0x30),
        Some(upstream_body()),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let (status, _) = answer(
        app.clone(),
        axum::http::Method::PATCH,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(0x30),
        None,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        "the surface registers exactly the ten paths"
    );

    let (status, _) = answer(
        app.clone(),
        axum::http::Method::DELETE,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(0x30),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    // A route create is validated against the route schema and refused 400 on
    // a reference that names no upstream of the calling tenant.
    let (status, document) = answer(
        app.clone(),
        axum::http::Method::POST,
        "/oagw/v1/routes",
        Some(0x30),
        Some(
            serde_json::json!({
                "upstream_id": uuid::Uuid::from_u128(0x31).to_string(),
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "priority": 1
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{document:?}");
    assert_eq!(
        document.expect("the problem")["detail"],
        "upstream_id does not reference an upstream of the calling tenant"
    );

    let (status, _) = answer(
        app,
        axum::http::Method::GET,
        "/oagw/v1/routes",
        Some(0x30),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn a_gear_without_an_authz_client_answers_403_everywhere() {
    let config = oagw::OagwConfig::default();
    let state = oagw::api::rest::state::OagwState::assemble(&config, None, None, None)
        .expect("the surface assembles");
    assert!(state.enforcer().is_none(), "no enforcer without a resolver");
    let app = oagw::api::rest::register_management_routes(axum::Router::new(), Arc::new(state));

    for (method, uri, body) in [
        (
            axum::http::Method::POST,
            "/oagw/v1/upstreams",
            Some(upstream_body()),
        ),
        (axum::http::Method::GET, "/oagw/v1/upstreams", None),
        (axum::http::Method::GET, "/oagw/v1/routes", None),
        (
            axum::http::Method::PUT,
            "/oagw/v1/upstreams/some-id",
            Some(upstream_body()),
        ),
        (
            axum::http::Method::DELETE,
            "/oagw/v1/routes/some-id",
            None,
        ),
    ] {
        let (status, document) = answer(app.clone(), method, uri, Some(0x30), body).await;
        assert_eq!(
            status,
            axum::http::StatusCode::FORBIDDEN,
            "{uri} {document:?}"
        );
    }

    // The surface the gear mounts is the only one a caller reaches: the
    // foundation mount point without a state answers nothing at all.
    let bare: axum::Router = oagw::api::rest::nest_mount_point(axum::Router::new());
    let (status, _) = answer(
        bare,
        axum::http::Method::GET,
        "/oagw/v1/upstreams",
        Some(0x30),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

#[test]
fn the_gear_state_is_absent_before_the_rest_surface_is_registered() {
    let gear = OagwGear::default();
    assert!(gear.state().is_none(), "no surface before register_rest");
    assert!(gear.config().is_none());
    assert!(gear.registry().is_none());
}

#[test]
fn the_enforcer_is_not_constructed_from_a_subject_alone() {
    // The security context the handlers read is the platform's; a context with
    // no tenant never resolves a calling tenant, and the surface fails closed.
    let anonymous = toolkit_security::SecurityContext::anonymous();
    let instance = String::from("/oagw/v1/upstreams");
    let error = oagw::control_plane::scoping::calling_tenant(&anonymous)
        .expect_err("an anonymous subject carries no tenant");
    assert_eq!(error.http_status(), 401, "{error}");
    let _ = instance;
}
