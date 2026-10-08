#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `GET /chats/{id}/turns/{request_id}` (D "Turn Status API", S§6.5).

mod common;

use axum::http::StatusCode;
use mini_chat::infra::db::entity::{chat, chat_turn};
use mini_chat::infra::db::repos::{ChatRepo, TurnRepo};
use uuid::Uuid;

use common::*;

const TURN_TYPE: &str = "gts.cf.core.mini_chat.turn.v1~";

struct Fixture {
    app: TestApp,
    user: Uuid,
    tenant: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        Self {
            app: TestApp::builder().build().await,
            user: Uuid::new_v4(),
            tenant: Uuid::new_v4(),
        }
    }

    async fn chat(&self) -> chat::Model {
        let id = create_chat(&self.app.as_user(self.user, self.tenant), "s1").await;
        let conn = self.app.db.conn().unwrap();
        ChatRepo
            .find_by_id(&conn, &tenant_scope(self.tenant, self.user), id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn insert(&self, t: chat_turn::Model) -> chat_turn::Model {
        let conn = self.app.db.conn().unwrap();
        TurnRepo
            .insert(&conn, &tenant_scope(self.tenant, self.user), t)
            .await
            .unwrap()
    }

    async fn status(&self, chat: Uuid, rid: Uuid) -> (StatusCode, serde_json::Value) {
        let resp = self
            .app
            .as_user(self.user, self.tenant)
            .get(&turn_path(chat, rid))
            .await;
        let body = resp.json();
        (resp.status, body)
    }
}

#[tokio::test]
async fn status_mapping_running_done_error_cancelled() {
    let f = Fixture::new().await;
    let c = f.chat().await;
    let msg = Uuid::new_v4();
    let running = f.insert(turn_row(&c, Uuid::new_v4(), "running")).await;
    let done = f
        .insert(chat_turn::Model {
            assistant_message_id: Some(msg),
            ..turn_row(&c, Uuid::new_v4(), "completed")
        })
        .await;
    let failed = f
        .insert(chat_turn::Model {
            error_code: Some("provider_error".to_owned()),
            ..turn_row(&c, Uuid::new_v4(), "failed")
        })
        .await;
    let cancelled = f.insert(turn_row(&c, Uuid::new_v4(), "cancelled")).await;

    for (t, state) in [
        (&running, "running"),
        (&done, "done"),
        (&failed, "error"),
        (&cancelled, "cancelled"),
    ] {
        let (status, body) = f.status(c.id, t.request_id).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], state, "{body}");
        assert_eq!(body["request_id"], t.request_id.to_string());
        assert!(body["updated_at"].is_string(), "{body}");
        assert!(body.get("chat_id").is_none(), "{body}");
    }
    let (_, body) = f.status(c.id, done.request_id).await;
    assert_eq!(body["assistant_message_id"], msg.to_string());
    let (_, body) = f.status(c.id, failed.request_id).await;
    assert_eq!(body["error_code"], "provider_error");
}

#[tokio::test]
async fn error_code_and_assistant_id_omitted_when_null() {
    let f = Fixture::new().await;
    let c = f.chat().await;
    let running = f.insert(turn_row(&c, Uuid::new_v4(), "running")).await;
    let failed = f
        .insert(chat_turn::Model {
            error_code: Some("orphan_timeout".to_owned()),
            ..turn_row(&c, Uuid::new_v4(), "failed")
        })
        .await;

    let (_, body) = f.status(c.id, running.request_id).await;
    let obj = body.as_object().unwrap();
    assert!(!obj.contains_key("error_code"), "{body}");
    assert!(!obj.contains_key("assistant_message_id"), "{body}");

    let (_, body) = f.status(c.id, failed.request_id).await;
    assert_eq!(body["error_code"], "orphan_timeout");
    assert!(
        !body
            .as_object()
            .unwrap()
            .contains_key("assistant_message_id"),
        "{body}"
    );
}

#[tokio::test]
async fn foreign_and_unknown_turn_404() {
    let f = Fixture::new().await;
    let c = f.chat().await;
    let t = f.insert(turn_row(&c, Uuid::new_v4(), "completed")).await;
    let deleted = f
        .insert(chat_turn::Model {
            deleted_at: Some(ts(1_700_000_100)),
            ..turn_row(&c, Uuid::new_v4(), "completed")
        })
        .await;

    let (status, body) = f.status(c.id, Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["context"]["resource_type"], TURN_TYPE);

    let (status, body) = f.status(c.id, deleted.request_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["context"]["resource_type"], TURN_TYPE);

    // Another user (same tenant) cannot see the turn.
    let resp = f
        .app
        .as_user(Uuid::new_v4(), f.tenant)
        .get(&turn_path(c.id, t.request_id))
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());

    // Non-UUID request id → 400 invalid_path_params.
    let resp = f
        .app
        .as_user(f.user, f.tenant)
        .get(&format!("/mini-chat/v1/chats/{}/turns/not-a-uuid", c.id))
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}
