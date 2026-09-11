//! The scripted-plugin source surface.
//!
//! A plugin definition is immutable: it is created once and can only be read or deleted.
//! Its source text is served separately, by `GET /plugins/{id}/source`, so a client can
//! fetch the script without pulling the whole definition.

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::*;
use toolkit::api::OpenApiRegistry;
use toolkit::api::OperationBuilder;
use toolkit_security::SecurityContext;

use crate::error::{ErrorKind, OagwError};

use super::handlers::ConcreteService;

/// The `OpenAPI` tag the source operation carries.
const TAG: &str = "Outbound API Gateway";

/// The body `GET /oagw/v1/plugins/{id}/source` answers with.
#[toolkit_macros::api_dto(response)]
pub struct PluginSource {
    /// The plugin identifier the source belongs to.
    pub id: String,
    /// The plugin's name, so a client can confirm what it fetched.
    pub name: String,
    /// The source text.
    pub source: String,
}

/// Builds the response body for one plugin's source.
#[must_use]
pub fn source_dto(entity: &crate::domain::plugin::Plugin) -> PluginSource {
    PluginSource {
        id: entity.id.clone(),
        name: entity.name.clone(),
        source: entity.source.clone(),
    }
}

/// Serves the source text of one plugin definition.
///
/// # Errors
///
/// Returns a not-found error when the identifier is outside the caller's tenant chain.
pub async fn get_plugin_source(
    Extension(svc): Extension<Arc<ConcreteService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let chain = super::handlers::chain(&svc, &ctx).await?;
    let entity = svc
        .store()
        .get_plugin(&id, &chain)
        .ok_or_else(|| not_found(&id))?;
    Ok(ok_json(source_dto(&entity)))
}

fn not_found(id: &str) -> OagwError {
    OagwError::new(ErrorKind::RouteNotFound, format!("plugin `{id}` was not found"))
}

/// Registers the source route.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's source")
        .description("Read the script text behind one plugin definition.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(get_plugin_source)
        .json_response_with_schema::<PluginSource>(
            openapi,
            StatusCode::OK,
            "The plugin's source text",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}
