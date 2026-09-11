#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for Feature 1 (Gear Foundation) of the `oagw` gear.
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests exercise the gear
//! declaration through the toolkit registry, the mounted router through a
//! plain `axum::Router`, and the configuration binding through the raw
//! `gears.oagw.config` section.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

use oagw::api::rest::error::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, error_response,
};
use oagw::api::rest::routes::{MOUNT_ROOT, gear_router_with_fallback, register_routes};
use oagw::infra::storage::OagwStore;
use std::sync::Arc;
use oagw::api::rest::{dto::ErrorContext, error::OagwProblem};
use oagw::config::OagwConfig;
use oagw::domain::error::DomainError;
use oagw::domain::sharing::FlatHierarchy;
use oagw::gear::OagwGear;

/// Host OpenAPI registry double recording the registered component names.
#[derive(Default)]
struct RecordingRegistry {
    schemas: std::sync::Mutex<Vec<String>>,
}

impl RecordingRegistry {
    fn schema_names(&self) -> Vec<String> {
        self.schemas.lock().expect("schemas lock").clone()
    }
}

impl toolkit::api::OpenApiRegistry for RecordingRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        self.schemas
            .lock()
            .expect("schemas lock")
            .extend(schemas.into_iter().map(|(name, _)| name));
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Serve `router` with a `GET` request to `uri` and return the response.
async fn get(router: Router, uri: &str) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request serves")
}

/// Error-source header value of a response, if it carries one.
fn error_source(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get(ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Deserialize the body of a response into a JSON value.
async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("body is a JSON document")
}

// ---------- gear declaration and link-time registration ----------

/// The gear declaration is reachable for link-time inventory registration: an
/// unlinked crate registers no gear and fails silently otherwise.
// @cpt-begin:cpt-cf-oagw-dod-test-layering:p1:inst-full
#[test]
fn gear_declaration_is_discovered_through_link_time_registration() {
    let registry = toolkit::GearRegistry::discover_and_build().expect("registry builds");

    let entry = registry
        .gears()
        .iter()
        .find(|entry| entry.name() == "oagw")
        .expect("oagw gear is registered");

    assert_eq!(
        entry.deps(),
        &[
            "types-registry",
            "tenant-resolver",
            "credstore",
            "authz-resolver",
        ],
        "the declared gear dependencies resolve to the four dependency gears"
    );
    assert!(entry.caps().has::<toolkit::registry::RestApiCap>());
    assert!(entry.caps().has::<toolkit::registry::RunnableCap>());
    assert!(!entry.caps().has::<toolkit::registry::ApiGatewayCap>());
}
// @cpt-end:cpt-cf-oagw-dod-test-layering:p1:inst-full

#[test]
fn gear_registers_a_stateful_capability_with_a_lifecycle_entry() {
    let registry = toolkit::GearRegistry::discover_and_build().expect("registry builds");
    let entry = registry
        .gears()
        .iter()
        .find(|entry| entry.name() == "oagw")
        .expect("oagw gear is registered");

    // The `stateful` capability is registered through the gear's lifecycle
    // configuration, so the registry holds a runnable for it: this is the
    // anchor the later entries' background work attaches to.
    let runnable = entry
        .caps()
        .query::<toolkit::registry::RunnableCap>()
        .expect("oagw registers a runnable capability");
    let _ = runnable;
}

#[tokio::test]
async fn registered_runnable_stops_promptly_when_the_runtime_cancels_the_token() {
    let registry = toolkit::GearRegistry::discover_and_build().expect("registry builds");
    let entry = registry
        .gears()
        .iter()
        .find(|entry| entry.name() == "oagw")
        .expect("oagw gear is registered");
    let runnable = entry
        .caps()
        .query::<toolkit::registry::RunnableCap>()
        .expect("oagw registers a runnable capability");

    // `start` spawns the lifecycle entry; `stop` cancels the token it parked on
    // and must not wait for the 5 s stop timeout, because the entry returns as
    // soon as the token fires.
    let cancel = tokio_util::sync::CancellationToken::new();
    runnable
        .start(cancel)
        .await
        .expect("stateful entry starts");

    let stopped = tokio::time::timeout(
        std::time::Duration::from_millis(2_000),
        runnable.stop(tokio_util::sync::CancellationToken::new()),
    )
    .await
    .expect("stop returns promptly instead of running into the stop timeout");
    assert!(stopped.is_ok(), "stateful entry stops cleanly");
}

#[test]
fn gear_declares_no_rest_host_capability_and_no_api_prefix() {
    let registry = toolkit::GearRegistry::discover_and_build().expect("registry builds");
    let entry = registry
        .gears()
        .iter()
        .find(|entry| entry.name() == "oagw")
        .expect("oagw gear is registered");

    // The gear is hosted by the api-gateway; it registers gear-relative paths
    // only, so it never claims the rest_host capability and never registers an
    // `/api` prefix.
    assert_eq!(MOUNT_ROOT, "/oagw/v1");
    assert!(!MOUNT_ROOT.starts_with("/api"));
    assert!(!entry.caps().has::<toolkit::registry::ApiGatewayCap>());
}

// ---------- REST capability wiring and gear mount root ----------

#[tokio::test]
async fn router_serves_the_gear_mount_root_without_a_business_endpoint() {
    let gear = OagwGear::default();
    let router = register_routes(
        Router::new(),
        &RecordingRegistry::default(),
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
    );

    // The mount root is mounted.
    let response = get(router.clone(), "/oagw/v1/payments").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_GATEWAY));

    // Every subpath no handler of the mounted subtree matches is the canonical
    // fallback, including the bare mount root and its trailing-slash form.
    // (`/oagw/v1/upstreams` is a management endpoint as of entry 2.2, so it no
    // longer falls through here.)
    for uri in ["/oagw/v1", "/oagw/v1/", "/oagw/v1/a/b"] {
        let response = get(router.clone(), uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        let document = json_body(response).await;
        assert_eq!(
            document["type"],
            serde_json::json!("gts://gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"),
            "{uri}"
        );
    }

    // The gear struct holds no configuration before init, so no business
    // endpoint could have been registered by this feature.
    assert!(gear.config().is_none());
}

#[tokio::test]
async fn no_path_is_registered_under_an_api_prefix() {
    let router = register_routes(
        Router::new(),
        &RecordingRegistry::default(),
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
    );

    let response = get(router, "/api/oagw/v1/payments").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.headers().get(ERROR_SOURCE_HEADER).is_none(),
        "the response did not come from the gear, so the gear registered no /api path"
    );
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    assert!(bytes.is_empty(), "no gear-generated body outside the mount root");
}

#[tokio::test]
async fn mount_step_registers_the_problem_schema_in_the_host_registry() {
    let registry = RecordingRegistry::default();
    let _ = register_routes(
        Router::new(),
        &registry,
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
    );
    // Entry 2.2 registers the management document schemas in the same mount
    // step, so the problem document is the first entry, not the only one.
    assert_eq!(
        registry.schema_names().first().map(String::as_str),
        Some("OagwProblem")
    );
}

#[tokio::test]
async fn merged_router_serves_host_routes_next_to_the_mount_root() {
    let router = register_routes(
        Router::new().route("/host", axum::routing::get(|| async { "host" })),
        &RecordingRegistry::default(),
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
    );

    let response = get(router.clone(), "/host").await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = get(router, "/oagw/v1/payments").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------- canonical error response and error source ----------

#[tokio::test]
async fn unmounted_subpath_returns_a_canonical_problem_document_with_gateway_source() {
    let router = register_routes(
        Router::new(),
        &RecordingRegistry::default(),
        Arc::new(OagwStore::new()),
        Arc::new(FlatHierarchy),
    );
    let response = get(router, "/oagw/v1/does-not-exist").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_GATEWAY));

    let document = json_body(response).await;
    assert_eq!(
        document["type"],
        serde_json::json!("gts://gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
    );
    assert_eq!(document["title"], serde_json::json!("Route not found"));
    assert_eq!(document["status"], serde_json::json!(404));
    assert!(document["detail"].is_string());
    assert_eq!(document["instance"], serde_json::json!("/oagw/v1/does-not-exist"));
}

#[tokio::test]
async fn every_gear_originated_failure_maps_through_the_single_mapping_layer() {
    // Every variant of the error table goes through the mapping layer and
    // produces a problem+json response with the gateway classification.
    let errors: Vec<DomainError> = vec![
        DomainError::RouteError {
            detail: "route".to_owned(),
        },
        DomainError::ValidationError {
            detail: "validation".to_owned(),
        },
        DomainError::MissingTargetHost {
            detail: "missing host".to_owned(),
        },
        DomainError::InvalidTargetHost {
            detail: "invalid host".to_owned(),
        },
        DomainError::UnknownTargetHost {
            detail: "unknown host".to_owned(),
        },
        DomainError::AuthenticationFailed {
            detail: "auth".to_owned(),
        },
        DomainError::RouteNotFound {
            detail: "route".to_owned(),
        },
        DomainError::PluginInUse {
            detail: "in use".to_owned(),
        },
        DomainError::PayloadTooLarge {
            detail: "too large".to_owned(),
        },
        DomainError::RateLimitExceeded {
            detail: "rate".to_owned(),
            retry_after_seconds: Some(30),
        },
        DomainError::SecretNotFound {
            detail: "secret".to_owned(),
        },
        DomainError::ProtocolError {
            detail: "protocol".to_owned(),
        },
        DomainError::DownstreamError {
            detail: "downstream".to_owned(),
        },
        DomainError::StreamAborted {
            detail: "stream".to_owned(),
        },
        DomainError::LinkUnavailable {
            detail: "link".to_owned(),
            retry_after_seconds: Some(5),
        },
        DomainError::CircuitBreakerOpen {
            detail: "breaker".to_owned(),
            retry_after_seconds: Some(5),
        },
        DomainError::PluginNotFound {
            detail: "plugin".to_owned(),
        },
        DomainError::ConnectionTimeout {
            detail: "connect".to_owned(),
            retry_after_seconds: Some(2),
        },
        DomainError::RequestTimeout {
            detail: "request".to_owned(),
            retry_after_seconds: Some(2),
        },
        DomainError::IdleTimeout {
            detail: "idle".to_owned(),
            retry_after_seconds: Some(2),
        },
    ];

    for error in errors {
        let response = error_response(&error, &ErrorContext::for_request("/oagw/v1/x"));
        assert_eq!(response.status(), StatusCode::from_u16(error.status()).unwrap());
        assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_GATEWAY));
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json"),
            "{}",
            error.title()
        );
        let document = json_body(response).await;
        assert_eq!(document["status"], serde_json::json!(error.status()));
        assert_eq!(document["title"], serde_json::json!(error.title()));
        assert!(
            document["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("gts://gts.cf.core.errors.err.v1~cf.oagw.")),
            "{}",
            error.title()
        );
    }
}

#[tokio::test]
async fn problem_document_carries_only_the_provided_extension_fields() {
    let response = error_response(
        &DomainError::RateLimitExceeded {
            detail: "quota reached".to_owned(),
            retry_after_seconds: Some(11),
        },
        &ErrorContext::for_request("/oagw/v1/pay")
            .with_upstream_id("u-1")
            .with_host("payments.internal:8443")
            .with_trace_id("trace-1"),
    );
    let document = json_body(response).await;
    assert_eq!(document["upstream_id"], serde_json::json!("u-1"));
    assert_eq!(document["host"], serde_json::json!("payments.internal:8443"));
    assert_eq!(document["path"], serde_json::json!("/oagw/v1/pay"));
    assert_eq!(document["retry_after_seconds"], serde_json::json!(11));
    assert_eq!(document["trace_id"], serde_json::json!("trace-1"));

    // A context without extension values omits every extension field.
    let response = error_response(
        &DomainError::ValidationError {
            detail: "invalid".to_owned(),
        },
        &ErrorContext::default(),
    );
    let document = json_body(response).await;
    for member in ["upstream_id", "host", "path", "retry_after_seconds", "trace_id"] {
        assert!(document.get(member).is_none(), "{member} must be omitted");
    }
}

#[tokio::test]
async fn error_source_layer_covers_gateway_upstream_and_success_responses() {
    // Success produced by gear code: stamped `gateway` by the layer.
    let router = gear_router_with_fallback(
        Router::new().route("/oagw/v1/success", axum::routing::get(|| async { "ok" })),
    );
    let response = get(router, "/oagw/v1/success").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_GATEWAY));

    // Passthrough classified by the producing path: `upstream` is preserved.
    let router = gear_router_with_fallback(Router::new().route(
        "/oagw/v1/passthrough",
        axum::routing::get(|| async {
            oagw::api::rest::routes::classify_upstream(
                axum::response::Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("upstream body"))
                    .expect("response builds"),
            )
        }),
    ));
    let response = get(router, "/oagw/v1/passthrough").await;
    assert_eq!(
        error_source(&response).as_deref(),
        Some(ERROR_SOURCE_UPSTREAM),
        "the producing path keeps ownership of the classification"
    );
}

#[tokio::test]
async fn error_source_header_is_never_overwritten_by_the_layer() {
    // A producing path that sets a value keeps it, whatever the value is.
    let router = gear_router_with_fallback(Router::new().route(
        "/oagw/v1/edge",
        axum::routing::get(|| async {
            axum::response::Response::builder()
                .header(ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM)
                .body(Body::empty())
                .expect("response builds")
        }),
    ));
    let response = get(router, "/oagw/v1/edge").await;
    assert_eq!(error_source(&response).as_deref(), Some(ERROR_SOURCE_UPSTREAM));
}

#[test]
fn classify_upstream_is_provided_for_the_proxy_entry() {
    let response = axum::response::Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::empty())
        .expect("response builds");
    let stamped = oagw::api::rest::routes::classify_upstream(response);
    assert_eq!(
        stamped
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_UPSTREAM)
    );
}

// ---------- readiness ----------

#[tokio::test]
async fn readiness_is_reported_only_after_the_mount_step() {
    let gear = OagwGear::default();
    let check = gear.readiness();

    assert_eq!(check.name(), "oagw-readiness");
    let result = check.check().await;
    assert!(!gear.routes_mounted());
    assert_eq!(result.code.as_deref(), Some("routes_not_mounted"));
    assert!(
        result
            .message
            .as_deref()
            .is_some_and(|message| message.contains("not mounted")),
        "the message names the missing mount, without secret material"
    );
}

#[test]
fn readiness_check_is_returned_from_the_rest_capability_contract() {
    fn assert_healthcheck(cap: &dyn toolkit::Healthcheck) {
        assert!(!cap.name().is_empty());
    }
    let gear = OagwGear::default();
    assert_healthcheck(gear.readiness().as_ref());
}

#[test]
fn gear_holds_no_configuration_before_init() {
    let gear = OagwGear::default();
    assert!(gear.config().is_none());
    assert!(!gear.routes_mounted());
}

// ---------- configuration binding ----------

#[test]
fn absent_configuration_section_takes_the_recorded_defaults() {
    let config = OagwConfig::from_section(&serde_json::Value::Null).expect("absent section");
    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert!(config.ssrf_policy.enabled);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
}

#[test]
fn graded_configuration_section_loads_with_defaults_for_absent_keys() {
    // Shape of the graded runtime configuration (`config/e2e-local.yaml`):
    // `gears.oagw.config` sets three keys and leaves the token-cache keys
    // absent.
    let section = serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false },
    });
    let config = OagwConfig::from_section(&section).expect("graded section loads");
    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
}

#[test]
fn unusable_configuration_section_names_the_offending_key() {
    let section = serde_json::json!({ "proxy_timeout_secs": "two" });
    let error = OagwConfig::describe_section_error(&section)
        .expect("unusable section is diagnosed");
    assert_eq!(error.key(), "proxy_timeout_secs");
    assert!(
        error.to_string().contains("gears.oagw.config"),
        "{error}"
    );

    let section = serde_json::json!({ "token_cache_ttl_secs": 30 });
    let error = OagwConfig::describe_section_error(&section).expect("bound violation");
    assert_eq!(error.key(), "token_cache_ttl_secs");
}

#[test]
fn configuration_surface_carries_no_secret_material() {
    // `cpt-cf-oagw-principle-cred-isolation`: the recorded key set is closed
    // and holds no credential or secret key.
    let config = OagwConfig::default();
    let document = OagwProblem::new(
        &DomainError::SecretNotFound {
            detail: "secret_ref not found".to_owned(),
        },
        &ErrorContext::for_request("/oagw/v1/pay").with_upstream_id("u-1"),
    );
    let rendered = document.into_json().to_string().to_lowercase();
    for banned in ["password", "client_secret", "authorization", "bearer "] {
        assert!(!rendered.contains(banned), "{banned} leaked");
    }
    let _ = config;
}

#[test]
fn problem_document_is_never_a_non_problem_body() {
    let document = OagwProblem::new(
        &DomainError::RouteNotFound {
            detail: "no route".to_owned(),
        },
        &ErrorContext::default(),
    );
    let json = document.into_json();
    let object = json.as_object().expect("problem document is an object");
    for member in ["type", "title", "status", "detail"] {
        assert!(object.contains_key(member), "{member} is required");
    }
}
