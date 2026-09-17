//! Registration of the proxy endpoint inside the gear's REST surface.
//!
//! The management routes mount at `/oagw/v1/...`; the proxy mounts at
//! `/oagw/v1/proxy/{*proxy_path}`, under the same version prefix and without
//! the platform-level `/api` prefix. One axum route serves every HTTP method,
//! and one OpenAPI operation per method documents it.
use axum::{Router, routing::any};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, state,
};

use super::handler;

const PROXY_TAG: &str = "OAGW Proxy";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers the proxy endpoint, additively, on the gear's router.
pub fn register_proxy(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = router.route(handler::PROXY_PATH, any(handler::proxy));
    declare(openapi);
    router
}

/// Declare the proxy operation in the OpenAPI document, one entry per method.
///
/// The router is registered once with a method-agnostic route; the document
/// still lists every method, because a caller reading the contract should see
/// that `POST /oagw/v1/proxy/payments/v1/charges` is as legal as a `GET`.
fn declare(openapi: &dyn OpenApiRegistry) {
    for method in handler::proxied_methods() {
        let id = format!("oagw.proxy_{}", method.as_str().to_ascii_lowercase());
        let builder = OperationBuilder::<state::Missing, state::Missing, ()>::new(
            method,
            handler::PROXY_PATH,
        )
        .operation_id(id)
        .summary("Proxy request")
        .description(
            "Forward a request to the upstream an alias resolves to. The path after the \
                 alias and the query string are matched against the upstream's routes and \
                 forwarded; the body is streamed. A failure the gateway itself raises comes \
                 back as an RFC 9457 problem document with `X-OAGW-Error-Source: gateway`; an \
                 answer the upstream produced — whatever its status — is passed through \
                 unchanged with `X-OAGW-Error-Source: upstream`.",
        )
        .tag(PROXY_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param(
            "proxy_path",
            "Upstream alias, an optional path segment and an optional query string",
        )
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Malformed proxy request");
        openapi.register_operation(builder.spec());
    }
}
