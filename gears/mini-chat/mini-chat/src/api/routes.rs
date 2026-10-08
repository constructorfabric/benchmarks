//! Route table: every area registers its operations; the shared state is an `Extension`.

use std::sync::Arc;

use toolkit::api::OpenApiRegistry;

use crate::api::handlers;
use crate::domain::services::AppServices;

/// Registers all mini-chat routes on the api-gateway router.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry, app: Arc<AppServices>) -> axum::Router {
    let router = handlers::chats::register(router, openapi);
    let router = handlers::messages::register(router, openapi);
    let router = handlers::stream::register(router, openapi);
    let router = handlers::turns::register(router, openapi);
    let router = handlers::attachments::register(router, openapi);
    let router = handlers::models::register(router, openapi);
    let router = handlers::quota::register(router, openapi);
    router.layer(axum::Extension(app))
}
