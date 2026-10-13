//! Turn status (`GET`) and delete (`DELETE`) of `/chats/{id}/turns/{request_id}`.

use std::sync::Arc;

use axum::extract::Extension;
use axum::response::IntoResponse;
use http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{License, V1};
use crate::api::dto::{TurnStatusResponse, TurnStatusState};
use crate::domain::services::AppServices;
use crate::domain::turns;

pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<TurnStatusResponse>> {
    let st = turns::get_status(&app, &ctx, chat_id, request_id).await?;
    let state = match st.state {
        "running" => TurnStatusState::Running,
        "done" => TurnStatusState::Done,
        "cancelled" => TurnStatusState::Cancelled,
        _ => TurnStatusState::Error,
    };
    Ok(axum::Json(TurnStatusResponse {
        request_id: st.request_id,
        state,
        error_code: st.error_code,
        assistant_message_id: st.assistant_message_id,
        updated_at: st.updated_at,
    }))
}

pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    turns::delete_turn(&app, &ctx, chat_id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers the turn routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = OperationBuilder::get(format!("{V1}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get authoritative turn status")
        .tag("Mini Chat")
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);
    OperationBuilder::delete(format!("{V1}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the latest turn")
        .tag("Mini Chat")
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}
