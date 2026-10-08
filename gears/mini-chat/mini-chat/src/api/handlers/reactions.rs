//! Reaction handlers: `mini_chat.put_reaction`, `mini_chat.delete_reaction`.

use std::sync::Arc;

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::dto::reactions::{MiniChatReactionDto, SetReactionReq};
use crate::api::state::AppServices;

/// `mini_chat.put_reaction`: sets or replaces the caller's reaction on an assistant message.
///
/// # Errors
/// 400 for an invalid reaction or a non-assistant target, 404 for a missing chat or message,
/// 422 for a body without `reaction`, 403 / 503 from the PDP.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    let reaction = svc
        .reactions
        .set(&ctx, chat_id, msg_id, &req.reaction)
        .await?;
    Ok(Json(reaction.into()))
}

/// `mini_chat.delete_reaction`: removes the caller's reaction (idempotent), 204.
///
/// # Errors
/// 400 for a non-assistant target, 404 for a missing chat or message, 403 / 503 from the PDP.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.reactions.remove(&ctx, chat_id, msg_id).await?;
    Ok(no_content())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::test_support::app::{TestApp, TestResponse, ctx};
    use crate::test_support::stream::{
        CHATS, answer, create_chat, reactions_of, script_provider, stream_uri,
    };

    const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";
    const MESSAGE_RESOURCE: &str = "gts.cf.core.mini_chat.message.v1~";

    fn user() -> SecurityContext {
        ctx(Uuid::new_v4(), Uuid::new_v4())
    }

    fn reaction_uri(chat: Uuid, message: impl std::fmt::Display) -> String {
        format!("{CHATS}/{chat}/messages/{message}/reaction")
    }

    async fn put(app: &TestApp, who: &SecurityContext, uri: &str, body: Value) -> TestResponse {
        app.call("PUT", uri, who, Some(body)).await
    }

    /// One completed turn; returns `(chat, user message id, assistant message id)`.
    async fn one_turn(app: &TestApp, who: &SecurityContext) -> (Uuid, Uuid, Uuid) {
        let chat = create_chat(app, who, None).await;
        script_provider(app, answer(&["Hi"], 3, 2));
        app.stream("POST", &stream_uri(chat), who, json!({"content": "q"}))
            .await
            .unwrap_or_else(|r| panic!("send rejected: {}", r.json));
        let list = app
            .call("GET", &format!("{CHATS}/{chat}/messages"), who, None)
            .await;
        let id = |i: usize| -> Uuid {
            list.json["items"][i]["id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        };
        (chat, id(0), id(1))
    }

    fn precondition(res: &TestResponse) -> (&str, &str) {
        let v = &res.json["context"]["violations"][0];
        (v["subject"].as_str().unwrap(), v["type"].as_str().unwrap())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one lifecycle scenario with its error cases
    async fn reactions_lifecycle() {
        let app = TestApp::builder().build().await;
        let who = user();
        let (chat, user_msg, assistant) = one_turn(&app, &who).await;
        let uri = reaction_uri(chat, assistant);
        let my_reaction = || async {
            app.call("GET", &format!("{CHATS}/{chat}/messages"), &who, None)
                .await
                .json["items"][1]["my_reaction"]
                .clone()
        };

        let liked = put(&app, &who, &uri, json!({"reaction": "like"})).await;
        assert_eq!(liked.status, 200, "{}", liked.json);
        assert_eq!(liked.json["message_id"], json!(assistant));
        assert_eq!(liked.json["reaction"], "like");
        time::OffsetDateTime::parse(
            liked.json["created_at"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("RFC 3339 created_at");
        assert_eq!(my_reaction().await, "like");

        let disliked = put(&app, &who, &uri, json!({"reaction": "dislike"})).await;
        assert_eq!(disliked.status, 200, "{}", disliked.json);
        assert_eq!(disliked.json["reaction"], "dislike");
        let rows = reactions_of(&app, assistant).await;
        assert_eq!(rows.len(), 1, "the reaction is replaced, not added");
        assert_eq!(rows[0].reaction, "dislike");
        // Replacing keeps the reaction's creation time; the response shows the stored row.
        let parse = |v: &Value| {
            time::OffsetDateTime::parse(
                v.as_str().unwrap(),
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap()
        };
        assert_eq!(
            parse(&disliked.json["created_at"]),
            crate::api::dto::timestamp::to_api(rows[0].created_at)
        );
        assert_eq!(
            parse(&disliked.json["created_at"]),
            parse(&liked.json["created_at"]),
            "created_at is the creation time of the reaction"
        );
        assert_eq!(rows[0].user_id, who.subject_id());
        assert_eq!(rows[0].tenant_id, who.subject_tenant_id());
        assert_eq!(my_reaction().await, "dislike");

        for _ in 0..2 {
            let removed = app.call("DELETE", &uri, &who, None).await;
            assert_eq!(removed.status, 204, "idempotent: {}", removed.json);
            assert!(removed.json.is_null());
        }
        assert!(reactions_of(&app, assistant).await.is_empty());
        assert!(my_reaction().await.is_null());

        // Only assistant messages take reactions, for PUT and DELETE alike.
        let on_user = reaction_uri(chat, user_msg);
        let rejected = put(&app, &who, &on_user, json!({"reaction": "like"})).await;
        assert_eq!(rejected.status, 400, "{}", rejected.json);
        assert_eq!(precondition(&rejected), ("reaction_target", "STATE"));
        let rejected = app.call("DELETE", &on_user, &who, None).await;
        assert_eq!(rejected.status, 400, "{}", rejected.json);
        assert_eq!(precondition(&rejected), ("reaction_target", "STATE"));
        assert!(reactions_of(&app, user_msg).await.is_empty());

        // The value is validated before anything else, even for a chat that is not the caller's.
        let invalid = put(&app, &who, &uri, json!({"reaction": "love"})).await;
        assert_eq!(invalid.status, 400, "{}", invalid.json);
        let v = &invalid.json["context"]["field_violations"][0];
        assert_eq!(
            (v["field"].as_str(), v["reason"].as_str()),
            (Some("reaction"), Some("INVALID_REACTION"))
        );
        let stranger = user();
        let invalid = put(&app, &stranger, &uri, json!({"reaction": "love"})).await;
        assert_eq!(
            invalid.status, 400,
            "validated before authz: {}",
            invalid.json
        );
        assert_eq!(
            invalid.json["context"]["field_violations"][0]["reason"],
            "INVALID_REACTION"
        );

        let missing = put(&app, &who, &uri, json!({})).await;
        assert_eq!(missing.status, 422, "{}", missing.json);

        let unknown = put(
            &app,
            &who,
            &reaction_uri(chat, Uuid::new_v4()),
            json!({"reaction": "like"}),
        )
        .await;
        assert_eq!(unknown.status, 404, "{}", unknown.json);
        assert_eq!(unknown.json["context"]["resource_type"], MESSAGE_RESOURCE);
        let unknown = app
            .call("DELETE", &reaction_uri(chat, Uuid::new_v4()), &who, None)
            .await;
        assert_eq!(unknown.status, 404, "{}", unknown.json);
        assert_eq!(unknown.json["context"]["resource_type"], MESSAGE_RESOURCE);

        for res in [
            put(&app, &stranger, &uri, json!({"reaction": "like"})).await,
            app.call("DELETE", &uri, &stranger, None).await,
        ] {
            assert_eq!(res.status, 404, "{}", res.json);
            assert_eq!(res.json["context"]["resource_type"], CHAT_RESOURCE);
        }
        assert!(reactions_of(&app, assistant).await.is_empty());

        let bad_path = put(
            &app,
            &who,
            &reaction_uri(chat, "nope"),
            json!({"reaction": "like"}),
        )
        .await;
        assert_eq!(bad_path.status, 400, "{}", bad_path.json);
    }
}
