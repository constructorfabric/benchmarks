//! Data plane route registration (DESIGN.md §3.2 “Proxy API”).
//!
//! Every proxied call is `/{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`.
//! The routes are declared through `OperationBuilder` like the management
//! API, with the composed [`DataPlaneServiceImpl`] attached as an extension
//! so the handlers stay stateless.

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use crate::api::rest::handlers;
use crate::domain::services::OagwService;
use crate::infra::proxy::DataPlaneServiceImpl;

/// License feature of the data plane.
///
/// The gear ships with the platform base feature: the marker is required by
/// `OperationBuilder`'s type state, the empty list keeps every deployment
/// allowed.
struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Attaches the data plane to the gear router.
///
/// # Errors
/// Returns an error when the outbound HTTP client cannot be built.
pub fn register_proxy_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: &OagwService,
) -> anyhow::Result<Router> {
    let data_plane = Arc::new(
        DataPlaneServiceImpl::new(
            service.config().clone(),
            service.control_plane().clone(),
            Arc::new(crate::domain::plugin::LiteralSecretResolver),
        )
        .context("composing the oagw data plane")?,
    );
    let limit = service.config().max_body_bytes;
    // The body limit is enforced (and reported in the 413 problem) by the
    // [`ProxyBody`] extractor, which reads it from this extension.
    let limit_report = axum::Extension(crate::api::rest::body::MaxBodyBytes(limit));
    Ok(register(router, openapi, limit_report, data_plane))
}

fn register(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    limit_report: axum::Extension<crate::api::rest::body::MaxBodyBytes>,
    data_plane: Arc<DataPlaneServiceImpl>,
) -> Router {
    // The data plane is composed on its own router so the body limit and the
    // data plane extension stay scoped to the proxy routes.
    let data_plane_router = Router::<()>::new();
    let data_plane_router = OperationBuilder::get("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_get")
        .summary("Proxy a GET request")
        .description(
            "Forward a GET request to the upstream the alias resolves to. The \
             path suffix and query string are appended to the upstream path of \
             the matched route.",
        )
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::proxy::proxy_get)
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(data_plane_router, openapi);

    let data_plane_router = OperationBuilder::post("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_post")
        .summary("Proxy a POST request")
        .description("Forward a POST request, with its body, to the matched upstream.")
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::proxy::proxy_post)
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(data_plane_router, openapi);

    let data_plane_router = OperationBuilder::put("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_put")
        .summary("Proxy a PUT request")
        .description("Forward a PUT request, with its body, to the matched upstream.")
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::proxy::proxy_put)
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(data_plane_router, openapi);

    let data_plane_router = OperationBuilder::patch("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_patch")
        .summary("Proxy a PATCH request")
        .description("Forward a PATCH request, with its body, to the matched upstream.")
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::proxy::proxy_patch)
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(data_plane_router, openapi);

    let data_plane_router = OperationBuilder::delete("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_delete")
        .summary("Proxy a DELETE request")
        .description("Forward a DELETE request to the matched upstream.")
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::proxy::proxy_delete)
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(data_plane_router, openapi);

    let data_plane_router = head_route(data_plane_router, openapi);
    let data_plane_router = options_route(data_plane_router, openapi);

    router.merge(
        data_plane_router
            .layer(axum::Extension(data_plane))
            .layer(limit_report),
    )
}

fn head_route(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // `OperationBuilder::handler` maps only the five body-carrying methods, so
    // HEAD is composed as an explicit `MethodRouter`.
    OperationBuilder::new(Method::HEAD, "/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_head")
        .summary("Proxy a HEAD request")
        .description(
            "Forward a HEAD request to the matched upstream; the response carries no body.",
        )
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .method_router(axum::routing::head(handlers::proxy::proxy_head))
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(router, openapi)
}

fn options_route(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::new(Method::OPTIONS, "/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_options")
        .summary("Proxy an OPTIONS request")
        .description("Forward an OPTIONS request to the matched upstream.")
        .tag(crate::api::rest::routes::management::API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .method_router(axum::routing::options(handlers::proxy::proxy_options))
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "The request was rejected by a policy",
        )
        .standard_errors(openapi)
        .register(router, openapi)
}
