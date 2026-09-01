//! Router assembly for the management-API integration tests.
//!
//! The gear's `RestApiCapability::register_rest` needs a `GearCtx`, which the
//! integration tests cannot build; this module builds the same router over an
//! explicitly supplied [`RegistryStore`] + [`OagwConfig`] pair, so the tests
//! exercise the real handler stack — extractors, [`ControlPlaneService`],
//! [`error_source_layer`] — without the runtime.
//!
//! Every fallible step stays fallible: the module never panics, so the
//! `expect`/`unwrap` budget of this crate remains inside the test files.
//!
//! [`RestApiCapability::register_rest`]: crate::gear::OagwGear::register_rest

use std::collections::HashMap;
use std::sync::Arc;

use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::{SecurityContext, SecurityContextBuildError};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::metrics::MetricsRegistry;
use crate::domain::rate_limit::RateLimiterRegistry;
use crate::domain::services::{ControlPlaneService, NoHierarchy, TenantHierarchy};
use crate::infra::plugin::{
    PluginRegistry, SecretResolverTrait, TokenCacheConfig, UnavailableSecretResolver,
};
use crate::infra::proxy::ProxyEngine;
use crate::infra::storage::{CacheLimits, RegistryStore};
use crate::{
    api::rest::error_source_layer, api::rest::handlers::proxy::DataPlane, api::rest::routes,
    domain::validation::Validator,
};

/// L1 budgets large enough that no test observes a flush.
#[must_use]
pub fn limits() -> CacheLimits {
    CacheLimits {
        upstream: 64,
        route: 64,
        plugin: 64,
        dp: 64,
    }
}

/// A management router and the service that owns its registry.
pub struct TestApp {
    /// Router to send requests to.
    pub router: axum::Router,
    /// Service backing the router, for direct assertions on the registry.
    pub service: Arc<ControlPlaneService>,
}

impl TestApp {
    /// Sends `request` to the router.
    ///
    /// # Errors
    ///
    /// The underlying `Router` service is infallible, so this never fails.
    pub async fn send(
        &mut self,
        request: axum::http::Request<axum::body::Body>,
    ) -> Result<axum::response::Response, std::convert::Infallible> {
        use tower::ServiceExt;
        self.router.clone().oneshot(request).await
    }
}

/// Builds a management router over a fresh registry and `hierarchy`.
#[must_use]
pub fn build_app(config: OagwConfig, hierarchy: Arc<dyn TenantHierarchy>) -> TestApp {
    let registry = Arc::new(RegistryStore::new(limits()));
    let service = Arc::new(ControlPlaneService::new(
        Arc::clone(&registry),
        Validator::new(config),
        hierarchy,
    ));
    let openapi = OpenApiRegistryImpl::new();
    let router = routes::register_routes(axum::Router::new(), &openapi)
        .layer(axum::Extension(Arc::clone(&service)))
        .layer(axum::middleware::from_fn(error_source_layer));
    TestApp { router, service }
}

/// Builds a management router without a tenant hierarchy.
#[must_use]
pub fn build_app_without_hierarchy(config: OagwConfig) -> TestApp {
    build_app(config, Arc::new(NoHierarchy))
}

/// A data-plane router and everything the tests assert against.
pub struct ProxyApp {
    /// Router carrying the proxy and metrics routes.
    pub router: axum::Router,
    /// Registry the tests seed upstreams and routes into.
    pub service: Arc<ControlPlaneService>,
    /// Outbound engine, for direct endpoint-selection assertions.
    pub engine: Arc<ProxyEngine>,
    /// Metrics the data plane recorded, for direct rendering assertions.
    pub metrics: Arc<MetricsRegistry>,
}

impl ProxyApp {
    /// Sends `request` to the router.
    ///
    /// # Errors
    ///
    /// The underlying `Router` service is infallible, so this never fails.
    pub async fn send(
        &mut self,
        request: axum::http::Request<axum::body::Body>,
    ) -> Result<axum::response::Response, std::convert::Infallible> {
        use tower::ServiceExt;
        self.router.clone().oneshot(request).await
    }
}

/// Builds the data-plane router over a fresh registry.
///
/// The management routes are mounted on the same router, so a test can seed a
/// resource over REST and then proxy to it, exactly as a caller would.
#[must_use]
pub fn build_proxy_app(config: OagwConfig, hierarchy: Arc<dyn TenantHierarchy>) -> ProxyApp {
    build_proxy_app_with_resolver(config, hierarchy, Arc::new(UnavailableSecretResolver))
}

/// Builds the data-plane router whose auth plugins resolve credentials from
/// `secrets` (keyed by the bare `cred://` reference, see
/// [`StaticSecretResolver`](crate::infra::plugin::StaticSecretResolver)).
#[must_use]
pub fn build_proxy_app_with_secrets(
    config: OagwConfig,
    hierarchy: Arc<dyn TenantHierarchy>,
    secrets: HashMap<String, String>,
) -> ProxyApp {
    build_proxy_app_with_resolver(
        config,
        hierarchy,
        Arc::new(crate::infra::plugin::StaticSecretResolver::new(secrets)),
    )
}

fn build_proxy_app_with_resolver(
    config: OagwConfig,
    hierarchy: Arc<dyn TenantHierarchy>,
    resolver: Arc<dyn SecretResolverTrait>,
) -> ProxyApp {
    let registry = Arc::new(RegistryStore::new(limits()));
    let service = Arc::new(ControlPlaneService::new(
        Arc::clone(&registry),
        Validator::new(config.clone()),
        hierarchy,
    ));
    let metrics = Arc::new(MetricsRegistry::new());
    let engine = Arc::new(ProxyEngine::new(&config, Arc::clone(&metrics)));
    let plugins = Arc::new(PluginRegistry::with_builtins(
        resolver,
        TokenCacheConfig::from(&config),
    ));
    // One registry for the data plane and the store, exactly as the gear
    // builder wires them: an upstream deleted in a test must lose its buckets.
    let rate_limiters = Arc::new(RateLimiterRegistry::default());
    registry.attach_rate_limiters(Arc::clone(&rate_limiters));
    let plane = DataPlane {
        service: Arc::clone(&service),
        engine: Arc::clone(&engine),
        metrics: Arc::clone(&metrics),
        plugins,
        rate_limiters,
    };
    let openapi = OpenApiRegistryImpl::new();
    let router = routes::register_routes(axum::Router::new(), &openapi)
        .layer(axum::Extension(Arc::clone(&service)))
        .layer(axum::middleware::from_fn(error_source_layer))
        .merge(
            routes::register_proxy_routes(axum::Router::new(), &openapi)
                .layer(axum::Extension(plane))
                .layer(axum::middleware::from_fn(error_source_layer)),
        );
    ProxyApp {
        router,
        service,
        engine,
        metrics,
    }
}

/// Builds a data-plane router without a tenant hierarchy.
#[must_use]
pub fn build_proxy_app_without_hierarchy(config: OagwConfig) -> ProxyApp {
    build_proxy_app(config, Arc::new(NoHierarchy))
}

/// Builds a proxy request for one data-plane call.
///
/// # Errors
///
/// Returns the underlying `http` error, which the test reports as a harness
/// failure.
pub fn proxy_request(
    method: &'static str,
    path: &str,
    ctx: SecurityContext,
    headers: &[(&'static str, &str)],
) -> Result<axum::http::Request<axum::body::Body>, axum::http::Error> {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .extension(ctx);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(axum::body::Body::empty())
}

/// Builds an anonymous proxy request: no `SecurityContext` extension, which is
/// how a browser preflight arrives (ADR-0004 "Preflight Request Handling").
///
/// # Errors
///
/// Returns the underlying `http` error, which the test reports as a harness
/// failure.
pub fn anonymous_request(
    method: &'static str,
    path: &str,
    headers: &[(&'static str, &str)],
) -> Result<axum::http::Request<axum::body::Body>, axum::http::Error> {
    let mut builder = axum::http::Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(axum::body::Body::empty())
}

/// Builds an `axum` request for one management call.
///
/// # Errors
///
/// Returns the underlying `http` error when the method/URI pair cannot be
/// assembled, which the test reports as a harness failure.
pub fn request(
    method: &'static str,
    path: &str,
    ctx: SecurityContext,
    body: Option<&str>,
) -> Result<axum::http::Request<axum::body::Body>, axum::http::Error> {
    let builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .extension(ctx);
    let Some(payload) = body else {
        return builder.body(axum::body::Body::empty());
    };
    builder
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(payload.to_owned()))
}

/// A security context for `tenant`.
///
/// # Errors
///
/// Returns the underlying builder error, which the test reports as a harness
/// failure.
pub fn caller(tenant: Uuid) -> Result<SecurityContext, SecurityContextBuildError> {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x101))
        .subject_tenant_id(tenant)
        .build()
}

/// Collects the response body as a UTF-8 string.
///
/// # Errors
///
/// Returns the underlying body error, which the test reports as a harness
/// failure.
pub async fn body(response: axum::response::Response) -> Result<String, axum::Error> {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}
