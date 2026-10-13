#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: `GET /v1/chats/{id}/turns/{request_id}`
//! (`cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`) over the real
//! stores: the response shape per state, the 404 cases, the path errors and
//! the authorization contract.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::chat_test_env::{CHAT_TYPE, Reply, request_id as seeded_request, ts};
use crate::infra::db::test_support::turn_row;
use crate::stream_test_env::{Options, StreamEnv, body, oa, rid};
use crate::test_doubles::{PdpReply, USER_TENANT, other_tenant_ctx, other_user_ctx};

const TURN_TYPE: &str = "gts.cf.core.mini_chat.turn.v1~";

fn keys(reply: &Reply) -> BTreeSet<String> {
    reply.json.as_object().unwrap().keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

async fn seed(
    env: &StreamEnv,
    n: u128,
    state: &str,
    edit: impl FnOnce(crate::infra::db::test_support::Row) -> crate::infra::db::test_support::Row,
) -> Uuid {
    let id = Uuid::from_u128(0xA900_0000 + n);
    env.quota
        .db
        .insert(
            "chat_turns",
            edit(
                turn_row(id, USER_TENANT, env.chat, seeded_request(n), state)
                    .set("updated_at", ts(100)),
            ),
        )
        .await
        .unwrap();
    seeded_request(n)
}

async fn status(env: &StreamEnv, request: Uuid) -> Reply {
    env.turn_status(request).await
}

#[tokio::test]
async fn a_running_turn_has_only_its_request_id_state_and_update_time() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "running", |row| row).await;
    let reply = status(&env, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply), set(&["request_id", "state", "updated_at"]));
    assert_eq!(reply.json["request_id"], request.to_string());
    assert_eq!(reply.json["state"], "running");
}

#[tokio::test]
async fn a_completed_turn_is_done_with_its_assistant_message_id() {
    let env = StreamEnv::new().await;
    let message = Uuid::from_u128(0xB900_0001);
    let request = seed(&env, 1, "completed", |row| {
        row.set("assistant_message_id", message)
    })
    .await;
    let reply = status(&env, request).await;
    assert_eq!(
        keys(&reply),
        set(&["request_id", "state", "assistant_message_id", "updated_at"])
    );
    assert_eq!(reply.json["state"], "done");
    assert_eq!(reply.json["assistant_message_id"], message.to_string());
}

#[tokio::test]
async fn a_failed_turn_is_an_error_with_its_stored_code_and_no_message() {
    let env = StreamEnv::new().await;
    for (n, code) in [
        "provider_error",
        "orphan_timeout",
        "web_search_calls_exceeded",
    ]
    .into_iter()
    .enumerate()
    {
        let request = seed(&env, n as u128 + 1, "failed", |row| {
            row.set("error_code", code)
        })
        .await;
        let reply = status(&env, request).await;
        assert_eq!(
            keys(&reply),
            set(&["request_id", "state", "error_code", "updated_at"]),
            "{code}"
        );
        assert_eq!(reply.json["state"], "error");
        assert_eq!(reply.json["error_code"], code);
    }
}

#[tokio::test]
async fn a_failed_turn_without_a_stored_code_has_no_error_code_key() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "failed", |row| row).await;
    let reply = status(&env, request).await;
    assert_eq!(reply.json["state"], "error");
    assert_eq!(keys(&reply), set(&["request_id", "state", "updated_at"]));
}

#[tokio::test]
async fn a_cancelled_turn_carries_the_message_only_when_a_partial_one_was_persisted() {
    let env = StreamEnv::new().await;
    let message = Uuid::from_u128(0xB900_0002);
    let with = seed(&env, 1, "cancelled", |row| {
        row.set("assistant_message_id", message)
    })
    .await;
    let without = seed(&env, 2, "cancelled", |row| row).await;
    let reply = status(&env, with).await;
    assert_eq!(reply.json["state"], "cancelled");
    assert_eq!(reply.json["assistant_message_id"], message.to_string());
    let reply = status(&env, without).await;
    assert_eq!(reply.json["state"], "cancelled");
    assert_eq!(keys(&reply), set(&["request_id", "state", "updated_at"]));
}

#[tokio::test]
async fn nothing_internal_is_ever_projected() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "failed", |row| {
        row.set("error_code", "provider_error")
            .set("error_detail", "secret detail resp_9xY2")
            .set("provider_response_id", "resp_internal")
            .set("reserve_tokens", 1500_i64)
            .set("reserved_credits_micro", 3_750_000_i64)
            .set("policy_version_applied", 7_i64)
            .set("effective_model", "p")
    })
    .await;
    let reply = status(&env, request).await;
    assert_eq!(
        keys(&reply),
        set(&["request_id", "state", "error_code", "updated_at"])
    );
    for forbidden in [
        "chat_id",
        "resp_",
        "secret detail",
        "reserve",
        "credits",
        "billing",
        "effective_model",
        "error_detail",
    ] {
        assert!(
            !reply.text.contains(forbidden),
            "{forbidden} in {}",
            reply.text
        );
    }
    assert!(!reply.text.contains(&env.chat.to_string()), "no chat id");
}

#[tokio::test]
async fn updated_at_is_rfc3339_utc() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "running", |row| row).await;
    let reply = status(&env, request).await;
    let stamp = reply.json["updated_at"].as_str().unwrap();
    assert!(stamp.ends_with('Z'), "{stamp}");
    let parsed = chrono::DateTime::parse_from_rfc3339(stamp).unwrap();
    assert_eq!(parsed.timestamp(), crate::chat_test_env::BASE_UNIX + 100);
}

// ---- 404 ----------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_turn_is_a_404_of_the_turn_type() {
    let env = StreamEnv::new().await;
    status(&env, rid(404)).await.assert_not_found(TURN_TYPE);
}

#[tokio::test]
async fn a_soft_deleted_turn_is_a_404_of_the_turn_type() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "completed", |row| row).await;
    env.quota
        .db
        .soft_delete("chat_turns", Uuid::from_u128(0xA900_0001))
        .await;
    status(&env, request).await.assert_not_found(TURN_TYPE);
}

#[tokio::test]
async fn a_turn_of_another_chat_is_a_404_of_the_turn_type() {
    let env = StreamEnv::new().await;
    let other = env.seed_chat(2, "p").await;
    let request = seed(&env, 1, "completed", |row| row).await;
    let reply = env.get(&StreamEnv::turn_uri(other, request)).await;
    reply.assert_not_found(TURN_TYPE);
}

#[tokio::test]
async fn a_missing_deleted_or_foreign_chat_is_a_404_of_the_chat_type() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "completed", |row| row).await;
    env.get(&StreamEnv::turn_uri(Uuid::from_u128(0xDEAD), request))
        .await
        .assert_not_found(CHAT_TYPE);
    env.get_as(&other_user_ctx(), &StreamEnv::turn_uri(env.chat, request))
        .await
        .assert_not_found(CHAT_TYPE);
    env.get_as(&other_tenant_ctx(), &StreamEnv::turn_uri(env.chat, request))
        .await
        .assert_not_found(CHAT_TYPE);
    env.quota.db.soft_delete("chats", env.chat).await;
    status(&env, request).await.assert_not_found(CHAT_TYPE);
}

// ---- path errors --------------------------------------------------------------------------------

#[tokio::test]
async fn a_request_id_that_is_not_a_uuid_is_400_invalid_path_params() {
    let env = StreamEnv::new().await;
    let reply = env
        .get(&format!(
            "/mini-chat/v1/chats/{}/turns/not-a-uuid",
            env.chat
        ))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.first_violation().1, "invalid_path_params");
    let reply = env
        .get(&format!("/mini-chat/v1/chats/not-a-uuid/turns/{}", rid(1)))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.first_violation().1, "invalid_path_params");
}

#[tokio::test]
async fn query_parameters_are_ignored() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "running", |row| row).await;
    let reply = env
        .get(&format!(
            "{}?x=1&$top=3",
            StreamEnv::turn_uri(env.chat, request)
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
}

// ---- authorization ------------------------------------------------------------------------------

#[tokio::test]
async fn the_status_is_authorized_as_read_turn_on_the_chat_and_nothing_else() {
    let env = StreamEnv::new().await;
    let request = seed(&env, 1, "running", |row| row).await;
    status(&env, request).await;
    let requests = env.quota.pdp.requests();
    assert_eq!(requests.len(), 1, "one evaluation");
    assert_eq!(requests[0].action.name, "read_turn");
    assert_eq!(requests[0].resource.id, Some(env.chat));
}

#[tokio::test]
async fn a_pdp_deny_is_403_and_a_pdp_failure_is_503_with_retry_after_5() {
    let env = StreamEnv::with(Options {
        pdp: PdpReply::Deny,
        ..Options::default()
    })
    .await;
    let request = seed(&env, 1, "running", |row| row).await;
    let reply = status(&env, request).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED");

    let env = StreamEnv::with(Options {
        pdp: PdpReply::Failure,
        ..Options::default()
    })
    .await;
    let request = seed(&env, 1, "running", |row| row).await;
    let reply = status(&env, request).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    assert_eq!(reply.header("retry-after"), Some("5"));
}

// ---- the lifecycle seen through the API ---------------------------------------------------------

#[tokio::test]
async fn the_status_follows_a_turn_from_running_to_done() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let request = rid(1);
    let mut stream = env.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    assert_eq!(status(&env, request).await.json["state"], "running");
    feed.send_event(oa::delta("x").0, &oa::delta("x").1);
    feed.send_event(oa::completed(1, 1).0, &oa::completed(1, 1).1);
    let frames = stream.rest().await;
    assert_eq!(frames.last().unwrap().name, "done");
    let reply = status(&env, request).await;
    assert_eq!(reply.json["state"], "done");
    assert!(reply.json["assistant_message_id"].as_str().is_some());
}

#[tokio::test]
async fn the_status_of_a_failed_stream_is_error_with_its_code() {
    let env = StreamEnv::new().await;
    env.script(&[oa::failed("boom")]);
    env.stream(env.chat, &body("Hi", rid(1))).await;
    let reply = status(&env, rid(1)).await;
    assert_eq!(reply.json["state"], "error");
    assert_eq!(reply.json["error_code"], "provider_error");
    assert!(reply.json.get("assistant_message_id").is_none());
}

#[tokio::test]
async fn the_status_of_a_disconnected_stream_is_cancelled() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("Hi", rid(1))).await;
    stream.next_frame().await.unwrap();
    feed.send_event(oa::delta("some").0, &oa::delta("some").1);
    stream.next_frame().await.unwrap();
    drop(stream);
    env.wait_terminal(rid(1)).await;
    let reply = status(&env, rid(1)).await;
    assert_eq!(reply.json["state"], "cancelled");
    assert!(
        reply.json["assistant_message_id"].as_str().is_some(),
        "the partial text was persisted"
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(json!(reply.json["request_id"]), json!(rid(1).to_string()));
}
