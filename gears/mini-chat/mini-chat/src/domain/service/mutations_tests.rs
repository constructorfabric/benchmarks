#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use futures::StreamExt;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::domain::error::{DomainError, reasons};
use crate::domain::service::stream::SendInput;
use crate::domain::service::stream::test_helpers::*;
use crate::domain::service::test_support::{
    Script, TENANT_A, TestEnv, USER_A1, USER_A2, ctx, ctx_a1, model,
};
use crate::infra::db::entity::{chat_turn, message, message_attachment, thread_summary};
use crate::infra::llm::{ContentPart, ToolSpec};

async fn send_ok(env: &TestEnv, chat: Uuid, i: SendInput) -> Uuid {
    let evs = collect(env.services.stream.send(&ctx_a1(), chat, i).await.unwrap()).await;
    assert_eq!(names(&evs).last(), Some(&"done"), "{evs:?}");
    started_request_id(&evs)
}

fn reason_of(e: &DomainError) -> String {
    match e {
        DomainError::InvalidArgument { reason, .. } | DomainError::OutOfRange { reason, .. } => {
            reason.clone()
        }
        DomainError::FailedPrecondition { kind, subject, .. } => format!("{subject}/{kind}"),
        DomainError::Aborted { reason, .. } | DomainError::PermissionDenied { reason } => {
            reason.clone()
        }
        DomainError::NotFound { resource } => format!("not_found:{resource}"),
        other => format!("{other:?}"),
    }
}

fn last_user_text(env: &TestEnv) -> String {
    let req = env.llm.requests.lock().last().cloned().unwrap();
    match &req.input.last().unwrap().content[0] {
        ContentPart::Text(t) => t.clone(),
        ContentPart::Image { .. } => panic!("image"),
    }
}

#[tokio::test]
async fn retry_replaces_latest_turn() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let old = send_ok(&env, chat, input("question")).await;

    let evs = collect(
        env.services
            .mutations
            .retry(&ctx_a1(), chat, old)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        names(&evs),
        vec!["stream_started", "delta", "delta", "done"]
    );
    let new_rid = started_request_id(&evs);
    assert_ne!(new_rid, old);
    assert_eq!(new_rid.get_version_num(), 4);
    assert_eq!(last_user_text(&env), "question");
    // The old turn's messages are not in the context of the retry.
    let req = env.llm.requests.lock().last().cloned().unwrap();
    assert_eq!(req.input.len(), 1);

    let o = turn(&env, chat, old).await;
    assert!(o.deleted_at.is_some());
    assert_eq!(o.replaced_by_request_id, Some(new_rid));
    let n = turn(&env, chat, new_rid).await;
    assert_eq!(n.state, "completed");
    assert!(n.reserve_tokens.is_some());
    assert_eq!(n.effective_model.as_deref(), Some("gpt-premium"));

    let msgs = messages(&env, chat).await;
    let live: Vec<_> = msgs.iter().filter(|m| m.deleted_at.is_none()).collect();
    assert_eq!(live.len(), 2);
    assert!(live.iter().all(|m| m.request_id == Some(new_rid)));
    assert_eq!(msgs.iter().filter(|m| m.deleted_at.is_some()).count(), 2);

    let audit = env
        .delivered_to(&env.deps.cfg.outbox.audit_queue_name, 3)
        .await;
    let retry = audit
        .iter()
        .find(|a| a["event_type"] == "turn_retry")
        .expect("turn_retry");
    assert_eq!(retry["original_request_id"], old.to_string());
    assert_eq!(retry["new_request_id"], new_rid.to_string());

    // The old request_id: 409 on send.
    let e = env
        .services
        .stream
        .send(
            &ctx_a1(),
            chat,
            SendInput {
                request_id: Some(old),
                ..input("q")
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::REQUEST_ID_CONFLICT);
    // Retrying the replaced turn: NOT_LATEST_TURN.
    let e = env
        .services
        .mutations
        .retry(&ctx_a1(), chat, old)
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::NOT_LATEST_TURN);
    env.shutdown().await;
}

#[tokio::test]
async fn edit_uses_new_content() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let first = send_ok(&env, chat, input("one")).await;
    let old = send_ok(&env, chat, input("two")).await;

    let e = env
        .services
        .mutations
        .edit(&ctx_a1(), chat, old, "  ".into())
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::EMPTY_CONTENT);
    let e = env
        .services
        .mutations
        .edit(&ctx_a1(), chat, first, "x".into())
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::NOT_LATEST_TURN);

    let evs = collect(
        env.services
            .mutations
            .edit(&ctx_a1(), chat, old, "new".into())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    assert_eq!(last_user_text(&env), "new");
    // History before the edited turn is kept.
    let req = env.llm.requests.lock().last().cloned().unwrap();
    assert_eq!(req.input.len(), 3);
    let live: Vec<String> = messages(&env, chat)
        .await
        .into_iter()
        .filter(|m| m.deleted_at.is_none())
        .map(|m| m.content)
        .collect();
    assert_eq!(live, vec!["one", "Hello world", "new", "Hello world"]);
    let audit = env
        .delivered_to(&env.deps.cfg.outbox.audit_queue_name, 4)
        .await;
    assert!(audit.iter().any(|a| a["event_type"] == "turn_edit"));
    env.shutdown().await;
}

#[tokio::test]
async fn delete_soft_deletes_turn_and_messages() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let first = send_ok(&env, chat, input("one")).await;
    let second = send_ok(&env, chat, input("two")).await;

    let e = env
        .services
        .mutations
        .delete(&ctx_a1(), chat, first)
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::NOT_LATEST_TURN);
    let e = env
        .services
        .mutations
        .delete(&ctx_a1(), chat, Uuid::new_v4())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, DomainError::NotFound { resource } if resource == crate::domain::error::resource_types::TURN)
    );

    env.services
        .mutations
        .delete(&ctx_a1(), chat, second)
        .await
        .unwrap();
    let t = turn(&env, chat, second).await;
    assert!(t.deleted_at.is_some());
    assert!(t.replaced_by_request_id.is_none());
    let msgs = messages(&env, chat).await;
    assert!(
        msgs.iter()
            .filter(|m| m.request_id == Some(second))
            .all(|m| m.deleted_at.is_some())
    );
    assert!(
        msgs.iter()
            .filter(|m| m.request_id == Some(first))
            .all(|m| m.deleted_at.is_none())
    );
    // Deleting it again: NOT_LATEST_TURN; the previous turn is the latest now.
    let e = env
        .services
        .mutations
        .delete(&ctx_a1(), chat, second)
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::NOT_LATEST_TURN);
    env.services
        .mutations
        .delete(&ctx_a1(), chat, first)
        .await
        .unwrap();

    let audit = env
        .delivered_to(&env.deps.cfg.outbox.audit_queue_name, 4)
        .await;
    let del: Vec<_> = audit
        .iter()
        .filter(|a| a["event_type"] == "turn_delete")
        .collect();
    assert_eq!(del.len(), 2);
    assert_eq!(del[0]["request_id"], second.to_string());
    assert_eq!(del[0]["actor_user_id"], USER_A1.to_string());
    env.shutdown().await;
}

#[tokio::test]
async fn running_turn_is_state_error_and_foreign_turn_is_403() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Hang(vec![]));
    let mut s = live(
        env.services
            .stream
            .send(&ctx_a1(), chat, input("x"))
            .await
            .unwrap(),
    )
    .into_events()
    .boxed();
    let rid = Uuid::parse_str(
        s.next().await.unwrap().data_json()["request_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for e in [
        env.services
            .mutations
            .delete(&ctx_a1(), chat, rid)
            .await
            .err()
            .unwrap(),
        env.services
            .mutations
            .retry(&ctx_a1(), chat, rid)
            .await
            .err()
            .unwrap(),
        env.services
            .mutations
            .edit(&ctx_a1(), chat, rid, "y".into())
            .await
            .err()
            .unwrap(),
    ] {
        assert_eq!(reason_of(&e), "turn_state/STATE");
    }
    drop(s);
    wait_terminal(&env, chat, rid).await;

    // A turn requested by another user (e.g. group attribution) cannot be mutated.
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::RequesterUserId,
            Expr::value(Some(USER_A2)),
        )
        .filter(chat_turn::Column::RequestId.eq(rid))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let e = env
        .services
        .mutations
        .delete(&ctx_a1(), chat, rid)
        .await
        .err()
        .unwrap();
    assert!(matches!(e, DomainError::PermissionDenied { .. }), "{e:?}");
    // Another user's chat is not visible.
    let e = env
        .services
        .mutations
        .delete(&ctx(USER_A2, TENANT_A), chat, rid)
        .await
        .err()
        .unwrap();
    assert!(matches!(e, DomainError::NotFound { .. }), "{e:?}");
    env.shutdown().await;
}

#[tokio::test]
async fn newer_running_turn_makes_target_not_latest() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let first = send_ok(&env, chat, input("one")).await;
    env.llm.push(Script::Hang(vec![]));
    let l = live(
        env.services
            .stream
            .send(&ctx_a1(), chat, input("two"))
            .await
            .unwrap(),
    );
    let e = env
        .services
        .mutations
        .retry(&ctx_a1(), chat, first)
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::NOT_LATEST_TURN);
    drop(l);
    env.shutdown().await;
}

#[tokio::test]
async fn concurrent_retries_one_wins() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let rid = send_ok(&env, chat, input("q")).await;
    env.llm.push(Script::Hang(vec![]));
    env.llm.push(Script::Hang(vec![]));
    let m = env.services.mutations.clone();
    let m2 = env.services.mutations.clone();
    let (a, b) = tokio::join!(
        async move { m.retry(&ctx_a1(), chat, rid).await },
        async move { m2.retry(&ctx_a1(), chat, rid).await }
    );
    let results = [a, b];
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "exactly one retry streams");
    for r in &results {
        if let Err(e) = r {
            let reason = reason_of(e);
            assert!(
                reason == reasons::GENERATION_IN_PROGRESS || reason == reasons::NOT_LATEST_TURN,
                "{e:?}"
            );
        }
    }
    drop(results);
    env.shutdown().await;
}

#[tokio::test]
async fn retry_carries_attachments_and_web_search() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let doc = add_attachment(&env, chat, TENANT_A, Att::default()).await;
    let img = add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            kind: "image",
            for_file_search: false,
            provider_file_id: Some("file-img00000000000009"),
            filename: "i.png",
            ..Att::default()
        },
    )
    .await;
    let gone = add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            filename: "gone.pdf",
            ..Att::default()
        },
    )
    .await;
    let old = send_ok(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![doc, img, gone],
            web_search: true,
            ..input("with files")
        },
    )
    .await;
    // The attachment deleted since the original turn is not carried forward.
    let conn = env.deps.db.conn().unwrap();
    crate::infra::db::entity::attachment::Entity::update_many()
        .col_expr(
            crate::infra::db::entity::attachment::Column::DeletedAt,
            Expr::value(Some(OffsetDateTime::now_utc())),
        )
        .filter(crate::infra::db::entity::attachment::Column::Id.eq(gone))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let evs = collect(
        env.services
            .mutations
            .retry(&ctx_a1(), chat, old)
            .await
            .unwrap(),
    )
    .await;
    let new_rid = started_request_id(&evs);
    let req = env.llm.requests.lock().last().cloned().unwrap();
    assert!(
        req.tools
            .iter()
            .any(|t| matches!(t, ToolSpec::WebSearch { .. }))
    );
    assert_eq!(
        req.input.last().unwrap().content,
        vec![
            ContentPart::Text("with files".into()),
            ContentPart::Image {
                file_id: "file-img00000000000009".into()
            }
        ]
    );
    assert!(turn(&env, chat, new_rid).await.web_search_enabled);

    let new_user = messages(&env, chat)
        .await
        .into_iter()
        .find(|m| m.request_id == Some(new_rid) && m.role == "user")
        .unwrap();
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::MessageId.eq(new_user.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    let mut linked: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
    linked.sort();
    let mut expected = vec![doc, img];
    expected.sort();
    assert_eq!(linked, expected);
    env.shutdown().await;
}

#[tokio::test]
async fn preflight_rejection_leaves_turn_untouched() {
    // A turn sent with web search; the kill switch is on at retry time: rejected before the
    // commit, nothing changes.
    let env = env_with(|o| o.kill_switches.disable_web_search = true).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let rid = send_ok(&env, chat, input("plain")).await;
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::WebSearchEnabled, Expr::value(true))
        .filter(chat_turn::Column::RequestId.eq(rid))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let e = env
        .services
        .mutations
        .retry(&ctx_a1(), chat, rid)
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), "web_search/FEATURE_DISABLED");
    let t = turn(&env, chat, rid).await;
    assert!(t.deleted_at.is_none());
    assert_eq!(turns(&env, chat).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn setup_failure_after_commit_marks_new_turn_failed() {
    let env = env_with(|o| {
        let mut m = model("tight", "premium");
        m.max_input_tokens = 0;
        m.context_window = 4096 + 200;
        m.estimation_budgets.fixed_overhead_tokens = 0;
        m.estimation_budgets.safety_margin_pct = 0;
        o.catalog.push(m);
    })
    .await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "tight").await;
    let old = send_ok(&env, chat, input("hi")).await;
    let e = env
        .services
        .mutations
        .edit(&ctx_a1(), chat, old, "x".repeat(2000))
        .await
        .err()
        .unwrap();
    assert_eq!(reason_of(&e), reasons::CONTEXT_BUDGET_EXCEEDED);
    let all = turns(&env, chat).await;
    assert_eq!(all.len(), 2);
    assert!(all[0].deleted_at.is_some());
    assert_eq!(all[1].state, "failed");
    assert_eq!(
        all[1].error_code.as_deref(),
        Some("context_length_exceeded")
    );
    assert!(all[1].reserve_tokens.is_none());
    // The chat is not blocked.
    send_ok(&env, chat, input("ok")).await;
    env.shutdown().await;
}

#[tokio::test]
async fn mutation_invalidates_covering_summary() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let first = send_ok(&env, chat, input("one")).await;
    let second = send_ok(&env, chat, input("two")).await;
    let msgs = messages(&env, chat).await;
    let first_asst = msgs
        .iter()
        .find(|m| m.request_id == Some(first) && m.role == "assistant")
        .unwrap();

    let conn = env.deps.db.conn().unwrap();
    let put_summary = |at: OffsetDateTime, id: Uuid| thread_summary::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat),
        summary_text: Set("sum".into()),
        summarized_up_to_created_at: Set(at),
        summarized_up_to_message_id: Set(id),
        token_estimate: Set(3),
        created_at: Set(OffsetDateTime::now_utc()),
        updated_at: Set(OffsetDateTime::now_utc()),
    };
    // Summary up to the first turn: kept on deleting the second turn.
    secure_insert::<thread_summary::Entity>(
        put_summary(first_asst.created_at, first_asst.id),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(message::Column::RequestId.eq(first))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    env.services
        .mutations
        .delete(&ctx_a1(), chat, second)
        .await
        .unwrap();
    assert!(summary_row(&env, chat).await.is_some());

    // Deleting the first turn (now latest, covered by the summary) drops the summary.
    env.services
        .mutations
        .delete(&ctx_a1(), chat, first)
        .await
        .unwrap();
    assert!(summary_row(&env, chat).await.is_none());
    assert!(messages(&env, chat).await.iter().all(|m| !m.is_compressed));
    env.shutdown().await;
}

#[tokio::test]
async fn http_turn_mutations() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let rid = send_ok(&env, chat, input("q")).await;
    let app = router(&env);

    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/turns/{rid}/retry"),
            None,
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let evs = parse_sse(&body_bytes(resp).await);
    assert_eq!(evs.first().unwrap().0, "stream_started");
    assert_eq!(evs.last().unwrap().0, "done");
    let new_rid = evs[0].1["request_id"].as_str().unwrap().to_owned();

    let resp = app
        .clone()
        .oneshot(request(
            "PATCH",
            &format!("/mini-chat/v1/chats/{chat}/turns/{new_rid}"),
            Some(serde_json::json!({"content": "edited"})),
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let evs = parse_sse(&body_bytes(resp).await);
    let edited_rid = evs[0].1["request_id"].as_str().unwrap().to_owned();

    let resp = app
        .clone()
        .oneshot(request(
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/turns/{rid}"),
            None,
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(body["context"]["reason"], "NOT_LATEST_TURN");

    let resp = app
        .clone()
        .oneshot(request(
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/turns/{edited_rid}"),
            None,
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    tokio::time::sleep(Duration::from_millis(10)).await;
    env.shutdown().await;
}

#[test]
fn setup_failure_codes() {
    let ctx_err = DomainError::out_of_range(
        crate::domain::error::resource_types::CHAT,
        "context",
        reasons::CONTEXT_BUDGET_EXCEEDED,
        "x",
    );
    assert_eq!(setup_failure_code(&ctx_err), "context_length_exceeded");
    assert_eq!(
        setup_failure_code(&DomainError::quota_exceeded("tokens")),
        "quota_exceeded"
    );
    assert_eq!(
        setup_failure_code(&DomainError::internal("x")),
        "turn_setup_failed"
    );
}

#[tokio::test]
async fn retry_and_edit_send_images_in_a_stable_order() {
    let env = env_with(|o| o.cfg.rag.max_images_per_message = 4).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let files = [
        "file-img00000000000031",
        "file-img00000000000032",
        "file-img00000000000033",
        "file-img00000000000034",
    ];
    let mut ids = Vec::new();
    for f in files {
        ids.push(
            add_attachment(
                &env,
                chat,
                TENANT_A,
                Att {
                    kind: "image",
                    for_file_search: false,
                    provider_file_id: Some(f),
                    filename: "i.png",
                    ..Att::default()
                },
            )
            .await,
        );
    }
    let file_of = |id: Uuid| files[ids.iter().position(|x| *x == id).unwrap()].to_owned();
    let images = |env: &TestEnv| -> Vec<String> {
        let req = env.llm.requests.lock().last().cloned().unwrap();
        req.input
            .last()
            .unwrap()
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Image { file_id } => Some(file_id.clone()),
                ContentPart::Text(_) => None,
            })
            .collect()
    };
    let mut rid = send_ok(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![ids[2], ids[0], ids[3], ids[1]],
            ..input("look")
        },
    )
    .await;
    // Links share created_at: retry / edit order them by (created_at, attachment_id).
    let mut sorted = ids.clone();
    sorted.sort();
    let expected: Vec<String> = sorted.iter().map(|id| file_of(*id)).collect();
    for i in 0..3 {
        let evs = if i == 1 {
            collect(
                env.services
                    .mutations
                    .edit(&ctx_a1(), chat, rid, "look again".into())
                    .await
                    .unwrap(),
            )
            .await
        } else {
            collect(
                env.services
                    .mutations
                    .retry(&ctx_a1(), chat, rid)
                    .await
                    .unwrap(),
            )
            .await
        };
        assert_eq!(names(&evs).last(), Some(&"done"));
        // The replaced turn's soft delete bumps its updated_at.
        let old = turn(&env, chat, rid).await;
        assert_eq!(Some(old.updated_at), old.deleted_at);
        rid = started_request_id(&evs);
        assert_eq!(images(&env), expected, "iteration {i}");
    }
    env.shutdown().await;
}
