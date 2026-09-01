//! REST handlers for calculator_gateway gear

use std::sync::Arc;

use axum::Extension;

use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use crate::domain::Service;

use super::dto::{AddRequest, AddResponse};

/// Handler for POST /calculator-gateway/v1/calculator/add
///
/// Accepts a JSON body with operands and returns their sum.
/// Delegates to Service directly.
pub async fn handle_add(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    Json(req): Json<AddRequest>,
) -> ApiResult<Json<AddResponse>> {
    let sum = service.add(&ctx, req.a, req.b).await.map_err(|e| {
        tracing::error!(error = %e, "addition failed");
        CanonicalError::internal(format!("Addition failed: {e}")).create()
    })?;

    Ok(Json(AddResponse { sum }))
}
