#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use futures::StreamExt;
use mini_chat_sdk::UsageTokens;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::AccessScope;
use tower::ServiceExt;
use uuid::Uuid;

use super::test_helpers::*;
use super::*;
use crate::domain::error::{DomainError, reasons, stream_codes};
use crate::domain::service::test_support::{
    FakeLlm, Script, TENANT_A, TENANT_B, TestEnv, USER_A1, USER_A2, USER_B, ctx, ctx_a1, model,
};
use crate::infra::db::entity::chat_turn;
use crate::infra::llm::{LlmCompletion, LlmEvent, LlmFailure, RawCitation, ToolSpec};

fn completed(text: &str, citations: Vec<RawCitation>) -> LlmEvent {
    LlmEvent::Completed(LlmCompletion {
        response_id: Some("resp_abc123456".into()),
        usage: Some(UsageTokens {
            input_tokens: 10,
            output_tokens: 5,
            ..UsageTokens::default()
        }),
        citations,
        output_text: text.into(),
        incomplete_reason: None,
    })
}

fn events(evs: Vec<LlmEvent>) -> Script {
    Script::Events {
        events: evs,
        delay: Duration::ZERO,
    }
}

async fn send(env: &TestEnv, chat: Uuid, i: SendInput) -> Result<StreamStart, DomainError> {
    env.services.stream.send(&ctx_a1(), chat, i).await
}

fn assert_reason(e: &DomainError, expected: &str) {
    let got = match e {
        DomainError::InvalidArgument { reason, .. } | DomainError::OutOfRange { reason, .. } => {
            reason.clone()
        }
        DomainError::FailedPrecondition { kind, .. } => kind.clone(),
        DomainError::Aborted { reason, .. } | DomainError::PermissionDenied { reason } => {
            reason.clone()
        }
        other => format!("{other:?}"),
    };
    assert_eq!(got, expected, "{e:?}");
}

#[tokio::test]
async fn happy_path_events_rows_and_outbox() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let evs = collect(send(&env, chat, input("Hi there")).await.unwrap()).await;
    assert_eq!(
        names(&evs),
        vec!["stream_started", "delta", "delta", "done"]
    );

    let started = data(&evs, "stream_started");
    assert_eq!(started["is_new_turn"], true);
    assert!(started.get("thread_summary_applied").is_none());
    let rid = started_request_id(&evs);
    assert_eq!(rid.get_version_num(), 4);
    let mid = Uuid::parse_str(started["message_id"].as_str().unwrap()).unwrap();

    let done = data(&evs, "done");
    assert_eq!(done["usage"]["input_tokens"], 100);
    assert_eq!(done["usage"]["output_tokens"], 50);
    assert_eq!(done["effective_model"], "gpt-premium");
    assert_eq!(done["selected_model"], "gpt-premium");
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("downgrade_from").is_none());
    assert!(!serde_json::to_string(&done).unwrap().contains("resp_"));

    let t = turn(&env, chat, rid).await;
    assert_eq!(t.state, "completed");
    assert_eq!(t.assistant_message_id, Some(mid));
    assert_eq!(t.provider_response_id.as_deref(), Some("resp_test123"));
    assert!(t.completed_at.is_some());
    assert!(t.error_code.is_none());
    assert_eq!(t.effective_model.as_deref(), Some("gpt-premium"));
    assert!(t.reserve_tokens.is_some());

    let msgs = messages(&env, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[0].content, "Hi there");
    assert_eq!(msgs[0].request_id, Some(rid));
    assert_eq!(msgs[1].id, mid);
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].content, "Hello world");
    assert_eq!(msgs[1].model.as_deref(), Some("gpt-premium"));
    assert_eq!((msgs[1].input_tokens, msgs[1].output_tokens), (100, 50));

    // Provider request shape.
    let req = env.llm.requests.lock()[0].clone();
    assert_eq!(req.model, "gpt-premium");
    assert!(req.stream);
    assert_eq!(req.user.len(), 64);
    assert_eq!(req.max_output_tokens, 4096);
    assert!(req.tools.is_empty());
    assert_eq!(req.max_tool_calls, None);
    assert_eq!(req.instructions, "You are a helpful assistant.");
    assert_eq!(req.metadata["request_type"], "chat");
    assert_eq!(req.metadata["feature"], "none");
    assert_eq!(req.metadata["chat_id"], chat.to_string());
    assert_eq!(req.input.len(), 1);

    let q = &env.deps.cfg.outbox;
    let usage = env.delivered_to(&q.queue_name, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["billing_outcome"], "completed");
    assert_eq!(usage[0]["settlement_method"], "actual");
    assert_eq!(usage[0]["terminal_state"], "completed");
    assert_eq!(usage[0]["requester_type"], "user");
    assert_eq!(
        usage[0]["dedupe_key"],
        format!("{}/{}/{}", TENANT_A.simple(), t.id.simple(), rid.simple())
    );
    let audit = env.delivered_to(&q.audit_queue_name, 1).await;
    assert_eq!(audit[0]["event_type"], "turn_completed");
    assert_eq!(audit[0]["policy_decisions"]["quota"]["decision"], "allow");
    env.shutdown().await;
}

#[tokio::test]
async fn history_is_included_in_next_request() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    collect(send(&env, chat, input("first")).await.unwrap()).await;
    collect(send(&env, chat, input("second")).await.unwrap()).await;
    let req = env.llm.requests.lock()[1].clone();
    let texts: Vec<String> = req
        .input
        .iter()
        .map(|m| match &m.content[0] {
            crate::infra::llm::ContentPart::Text(t) => format!("{}:{t}", m.role.as_str()),
            crate::infra::llm::ContentPart::Image { .. } => "image".to_owned(),
        })
        .collect();
    assert_eq!(
        texts,
        vec!["user:first", "assistant:Hello world", "user:second"]
    );
    env.shutdown().await;
}

#[tokio::test]
async fn replay_is_side_effect_free() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let rid = Uuid::new_v4();
    let first = collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(rid),
                ..input("Hi")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    let q = env.deps.cfg.outbox.clone();
    assert_eq!(env.delivered_to(&q.queue_name, 1).await.len(), 1);
    assert_eq!(env.delivered_to(&q.audit_queue_name, 1).await.len(), 1);

    let start = send(
        &env,
        chat,
        SendInput {
            request_id: Some(rid),
            ..input("Hi")
        },
    )
    .await
    .unwrap();
    assert!(matches!(start, StreamStart::Replay(_)));
    let evs = collect(start).await;
    assert_eq!(names(&evs), vec!["stream_started", "delta", "done"]);
    let s = data(&evs, "stream_started");
    assert_eq!(s["is_new_turn"], false);
    assert_eq!(
        s["message_id"],
        data(&first, "stream_started")["message_id"]
    );
    assert_eq!(s["request_id"], rid.to_string());
    assert_eq!(data(&evs, "delta")["content"], "Hello world");
    let done = data(&evs, "done");
    assert_eq!(done["usage"]["input_tokens"], 100);
    assert_eq!(done["quota_decision"], "allow");
    assert!(done.get("quota_warnings").is_none());
    assert_eq!(env.llm.requests.lock().len(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.delivered_to(&q.queue_name, 0).await.len(), 1);
    assert_eq!(env.delivered_to(&q.audit_queue_name, 0).await.len(), 1);
    assert_eq!(messages(&env, chat).await.len(), 2);
    env.shutdown().await;
}

#[tokio::test]
async fn request_id_conflicts_for_failed_cancelled_running() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;

    // failed
    env.llm.push(Script::Fail(LlmFailure {
        code: stream_codes::PROVIDER_ERROR,
        message: "boom".into(),
        usage: None,
        response_id: None,
        context_length_exceeded: false,
    }));
    let failed = Uuid::new_v4();
    collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(failed),
                ..input("a")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    let e = send(
        &env,
        chat,
        SendInput {
            request_id: Some(failed),
            ..input("a")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::REQUEST_ID_CONFLICT);

    // running
    env.llm
        .push(Script::Hang(vec![LlmEvent::TextDelta("partial".into())]));
    let running = Uuid::new_v4();
    let l = live(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(running),
                ..input("b")
            },
        )
        .await
        .unwrap(),
    );
    let e = send(
        &env,
        chat,
        SendInput {
            request_id: Some(running),
            ..input("b")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::REQUEST_ID_CONFLICT);

    // cancelled
    drop(l);
    let t = wait_terminal(&env, chat, running).await;
    assert_eq!(t.state, "cancelled");
    let e = send(
        &env,
        chat,
        SendInput {
            request_id: Some(running),
            ..input("b")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::REQUEST_ID_CONFLICT);
    env.shutdown().await;
}

#[tokio::test]
async fn parallel_guard_and_replay_before_guard() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let a = Uuid::new_v4();
    collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(a),
                ..input("A")
            },
        )
        .await
        .unwrap(),
    )
    .await;

    env.llm.push(Script::Hang(vec![]));
    let mut l = live(send(&env, chat, input("B")).await.unwrap())
        .into_events()
        .boxed();
    assert_eq!(l.next().await.unwrap().name(), "stream_started");

    let e = send(&env, chat, input("C")).await.err().unwrap();
    assert_reason(&e, reasons::TURN_ALREADY_RUNNING);
    // Replay of A is served although B is running.
    let r = send(
        &env,
        chat,
        SendInput {
            request_id: Some(a),
            ..input("A")
        },
    )
    .await
    .unwrap();
    assert!(matches!(r, StreamStart::Replay(_)));

    drop(l);
    let b = turns(&env, chat).await[1].clone();
    wait_terminal(&env, chat, b.request_id).await;
    let evs = collect(send(&env, chat, input("D")).await.unwrap()).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    env.shutdown().await;
}

#[tokio::test]
async fn preflight_validation_errors() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;

    let e = send(&env, chat, input("   ")).await.err().unwrap();
    assert_reason(&e, reasons::EMPTY_CONTENT);

    let id = Uuid::new_v4();
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![id, id],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::INVALID_ATTACHMENT);
    let many: Vec<Uuid> = (0..55).map(|_| Uuid::new_v4()).collect();
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: many,
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::INVALID_ATTACHMENT);

    // Unknown chat, foreign chat (other user, other tenant).
    let e = send(&env, Uuid::new_v4(), input("x")).await.err().unwrap();
    assert!(matches!(e, DomainError::NotFound { .. }), "{e:?}");
    let e = env
        .services
        .stream
        .send(&ctx(USER_A2, TENANT_A), chat, input("x"))
        .await
        .err()
        .unwrap();
    assert!(matches!(e, DomainError::NotFound { .. }), "{e:?}");
    let e = env
        .services
        .stream
        .send(&ctx(USER_B, TENANT_B), chat, input("x"))
        .await
        .err()
        .unwrap();
    assert!(matches!(e, DomainError::NotFound { .. }), "{e:?}");

    // Model removed from the catalog.
    let gone = create_chat(&env, USER_A1, TENANT_A, "gpt-removed").await;
    let e = send(&env, gone, input("x")).await.err().unwrap();
    assert_reason(&e, reasons::INVALID_MODEL);

    // Attachment of another chat: rejected in the reserve transaction, nothing remains.
    let other = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let att = add_attachment(&env, other, TENANT_A, Att::default()).await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![att],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::INVALID_ATTACHMENT);
    // Not-ready attachment of this chat.
    let pending = add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            status: "pending",
            ..Att::default()
        },
    )
    .await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![pending],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::INVALID_ATTACHMENT);
    // Attachment uploaded by someone else.
    let foreign = add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            uploaded_by: USER_A2,
            ..Att::default()
        },
    )
    .await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![foreign],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::INVALID_ATTACHMENT);

    assert!(messages(&env, chat).await.is_empty());
    assert!(turns(&env, chat).await.is_empty());
    assert!(env.llm.requests.lock().is_empty());

    // A valid attachment is linked to the user message.
    let ok = add_attachment(&env, chat, TENANT_A, Att::default()).await;
    let evs = collect(
        send(
            &env,
            chat,
            SendInput {
                attachment_ids: vec![ok],
                ..input("x")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    env.shutdown().await;
}

#[tokio::test]
async fn web_search_kill_switch_rejects() {
    let env = env_with(|o| o.kill_switches.disable_web_search = true).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let e = send(
        &env,
        chat,
        SendInput {
            web_search: true,
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    match &e {
        DomainError::FailedPrecondition { subject, kind, .. } => {
            assert_eq!(subject, "web_search");
            assert_eq!(kind, reasons::FEATURE_DISABLED);
        }
        other => panic!("{other:?}"),
    }
    // Without web search the request proceeds.
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    env.shutdown().await;
}

#[tokio::test]
async fn image_guards_and_image_input() {
    let env = env_with(|o| o.cfg.rag.max_images_per_message = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let img = |f: &'static str| Att {
        kind: "image",
        for_file_search: false,
        provider_file_id: Some(f),
        filename: "a.png",
        ..Att::default()
    };
    let i1 = add_attachment(&env, chat, TENANT_A, img("file-img00000000000001")).await;
    let i2 = add_attachment(&env, chat, TENANT_A, img("file-img00000000000002")).await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![i1, i2],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::TOO_MANY_IMAGES);

    let evs = collect(
        send(
            &env,
            chat,
            SendInput {
                attachment_ids: vec![i1],
                ..input("look")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    let req = env.llm.requests.lock().last().cloned().unwrap();
    let last = req.input.last().unwrap();
    assert_eq!(
        last.content,
        vec![
            crate::infra::llm::ContentPart::Text("look".into()),
            crate::infra::llm::ContentPart::Image {
                file_id: "file-img00000000000001".into()
            }
        ]
    );
    // Images of earlier turns are not re-sent.
    collect(send(&env, chat, input("again")).await.unwrap()).await;
    let req = env.llm.requests.lock().last().cloned().unwrap();
    assert!(req.input.iter().all(|m| m.content.len() == 1));
    env.shutdown().await;

    let env = env_with(|o| o.kill_switches.disable_images = true).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let i = add_attachment(&env, chat, TENANT_A, img("file-img00000000000003")).await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![i],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    match &e {
        DomainError::FailedPrecondition { subject, .. } => assert_eq!(subject, "images"),
        other => panic!("{other:?}"),
    }
    env.shutdown().await;

    let env = env_with(|o| {
        let mut m = model("text-only", "premium");
        m.multimodal_capabilities.clear();
        o.catalog.push(m);
    })
    .await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "text-only").await;
    let i = add_attachment(&env, chat, TENANT_A, img("file-img00000000000004")).await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![i],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::VISION_NOT_SUPPORTED);
    env.shutdown().await;
}

#[tokio::test]
async fn input_too_long_and_context_budget() {
    let env = env_with(|o| {
        let mut small = model("small-input", "premium");
        small.max_input_tokens = 200;
        o.catalog.push(small);
        let mut tiny = model("tiny-window", "premium");
        tiny.max_input_tokens = 0;
        tiny.context_window = 4096 + 150;
        o.catalog.push(tiny);
    })
    .await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "small-input").await;
    let e = send(&env, chat, input(&"x".repeat(2000)))
        .await
        .err()
        .unwrap();
    assert_reason(&e, reasons::INPUT_TOO_LONG);

    let chat = create_chat(&env, USER_A1, TENANT_A, "tiny-window").await;
    let e = send(&env, chat, input("hello")).await.err().unwrap();
    assert_reason(&e, reasons::CONTEXT_BUDGET_EXCEEDED);
    assert!(turns(&env, chat).await.is_empty());
    assert!(env.llm.requests.lock().is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn provider_failure_is_sanitized_and_turn_failed() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(events(vec![
        LlmEvent::TextDelta("par".into()),
        LlmEvent::Failed(LlmFailure {
            code: stream_codes::PROVIDER_ERROR,
            message: "bad file file-abcdefghijklmnop at https://x.y/z".into(),
            usage: None,
            response_id: None,
            context_length_exceeded: false,
        }),
    ]));
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    assert_eq!(names(&evs), vec!["stream_started", "delta", "error"]);
    let err = data(&evs, "error");
    assert_eq!(err["code"], "provider_error");
    assert_eq!(err["message"], "bad file [provider_id] at [url]");
    let rid = started_request_id(&evs);
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("provider_error"));
    assert!(t.assistant_message_id.is_none());
    assert_eq!(messages(&env, chat).await.len(), 1);
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(usage[0]["billing_outcome"], "failed");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    let audit = env
        .delivered_to(&env.deps.cfg.outbox.audit_queue_name, 1)
        .await;
    assert_eq!(audit[0]["event_type"], "turn_failed");
    assert_eq!(audit[0]["error_code"], "provider_error");
    env.shutdown().await;
}

#[tokio::test]
async fn pre_stream_rate_limited() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Fail(LlmFailure {
        code: stream_codes::RATE_LIMITED,
        message: "Rate limited, retry after 7 seconds".into(),
        usage: None,
        response_id: None,
        context_length_exceeded: false,
    }));
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    assert_eq!(names(&evs), vec!["stream_started", "error"]);
    assert_eq!(data(&evs, "error")["code"], "rate_limited");
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert_eq!(t.error_code.as_deref(), Some("rate_limited"));
    env.shutdown().await;
}

#[tokio::test]
async fn failure_with_usage_settles_actual() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(events(vec![LlmEvent::Failed(LlmFailure {
        code: stream_codes::PROVIDER_ERROR,
        message: "x".into(),
        usage: Some(UsageTokens {
            input_tokens: 3,
            output_tokens: 1,
            ..UsageTokens::default()
        }),
        response_id: Some("resp_failed0001".into()),
        context_length_exceeded: false,
    })]));
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert_eq!(t.provider_response_id.as_deref(), Some("resp_failed0001"));
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(usage[0]["settlement_method"], "actual");
    assert_eq!(usage[0]["usage"]["input_tokens"], 3);
    env.shutdown().await;
}

#[tokio::test]
async fn client_disconnect_cancels_with_partial_content() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Hang(vec![
        LlmEvent::TextDelta("partial ".into()),
        LlmEvent::TextDelta("answer".into()),
    ]));
    let mut s = live(send(&env, chat, input("x")).await.unwrap())
        .into_events()
        .boxed();
    let first = s.next().await.unwrap();
    let rid = Uuid::parse_str(first.data_json()["request_id"].as_str().unwrap()).unwrap();
    assert_eq!(s.next().await.unwrap().name(), "delta");
    assert_eq!(s.next().await.unwrap().name(), "delta");
    drop(s);
    let t = wait_terminal(&env, chat, rid).await;
    assert_eq!(t.state, "cancelled");
    let mid = t.assistant_message_id.expect("partial message");
    let msgs = messages(&env, chat).await;
    let m = msgs.iter().find(|m| m.id == mid).unwrap();
    assert_eq!(m.content, "partial answer");
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(usage[0]["billing_outcome"], "aborted");
    assert_eq!(usage[0]["settlement_method"], "estimated");
    assert_eq!(usage[0]["terminal_state"], "cancelled");
    env.shutdown().await;
}

#[tokio::test]
async fn disconnect_before_text_persists_no_message() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Hang(vec![]));
    let mut s = live(send(&env, chat, input("x")).await.unwrap())
        .into_events()
        .boxed();
    let rid = Uuid::parse_str(
        s.next().await.unwrap().data_json()["request_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    drop(s);
    let t = wait_terminal(&env, chat, rid).await;
    assert_eq!(t.state, "cancelled");
    assert!(t.assistant_message_id.is_none());
    assert_eq!(messages(&env, chat).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn tool_events_citations_and_web_search() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(events(vec![
        LlmEvent::ToolStart {
            name: "web_search".into(),
            details: serde_json::json!({}),
        },
        LlmEvent::ToolDone {
            name: "web_search".into(),
            details: serde_json::json!({}),
        },
        LlmEvent::TextDelta("See source".into()),
        completed(
            "See source",
            vec![RawCitation::Web {
                url: "https://example.com".into(),
                title: "Example".into(),
                snippet: "source".into(),
                span: Some((4, 10)),
            }],
        ),
    ]));
    let evs = collect(
        send(
            &env,
            chat,
            SendInput {
                web_search: true,
                ..input("x")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        names(&evs),
        vec![
            "stream_started",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ]
    );
    assert_eq!(evs[1].data_json()["phase"], "start");
    assert_eq!(evs[2].data_json()["phase"], "done");
    let c = data(&evs, "citations");
    assert_eq!(c["items"][0]["source"], "web");
    assert_eq!(c["items"][0]["url"], "https://example.com");
    assert_eq!(c["items"][0]["span"]["start"], 4);

    let req = env.llm.requests.lock()[0].clone();
    assert_eq!(
        req.tools,
        vec![ToolSpec::WebSearch {
            search_context_size: "low".into()
        }]
    );
    assert_eq!(req.max_tool_calls, Some(2));
    assert_eq!(req.metadata["feature"], "web_search");
    assert!(
        req.instructions
            .ends_with(crate::config::DEFAULT_WEB_SEARCH_GUARD)
    );
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert_eq!(t.web_search_completed_count, 1);
    assert!(t.web_search_enabled);
    env.shutdown().await;
}

#[tokio::test]
async fn web_search_call_limit_fails_turn() {
    let env = env_with(|o| o.cfg.quota.web_search_max_calls_per_message = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let start = |n: &str| LlmEvent::ToolStart {
        name: n.into(),
        details: serde_json::json!({}),
    };
    env.llm
        .push(Script::Hang(vec![start("web_search"), start("web_search")]));
    let evs = collect(
        send(
            &env,
            chat,
            SendInput {
                web_search: true,
                ..input("x")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(names(&evs), vec!["stream_started", "tool", "error"]);
    assert_eq!(data(&evs, "error")["code"], "web_search_calls_exceeded");
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("web_search_calls_exceeded"));
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(usage[0]["settlement_method"], "estimated");
    env.shutdown().await;
}

#[tokio::test]
async fn code_interpreter_call_limit_fails_turn() {
    let env = env_with(|o| o.cfg.quota.code_interpreter_max_calls_per_message = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            for_file_search: false,
            for_code_interpreter: true,
            provider_file_id: Some("file-xlsx000000000001"),
            filename: "a.xlsx",
            ..Att::default()
        },
    )
    .await;
    let start = LlmEvent::ToolStart {
        name: "code_interpreter".into(),
        details: serde_json::json!({}),
    };
    env.llm.push(Script::Hang(vec![start.clone(), start]));
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    assert_eq!(
        data(&evs, "error")["code"],
        "code_interpreter_calls_exceeded"
    );
    let req = env.llm.requests.lock()[0].clone();
    assert_eq!(
        req.tools,
        vec![ToolSpec::CodeInterpreter {
            file_ids: vec!["file-xlsx000000000001".into()]
        }]
    );
    env.shutdown().await;
}

#[tokio::test]
async fn file_search_tool_and_file_citations() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let doc = add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            provider_file_id: Some("file-known0000000001"),
            filename: "Q3 Report.pdf",
            ..Att::default()
        },
    )
    .await;
    add_vector_store(&env, chat, TENANT_A, "vs_chat0000000000001").await;
    env.llm.push(events(vec![
        LlmEvent::ToolDone {
            name: "file_search".into(),
            details: serde_json::json!({"files_searched": 0}),
        },
        LlmEvent::TextDelta("From the report".into()),
        completed(
            "From the report",
            vec![
                RawCitation::File {
                    file_id: "file-known0000000001".into(),
                    filename: Some("x".into()),
                    span: None,
                },
                RawCitation::File {
                    file_id: "file-unknown00000001".into(),
                    filename: None,
                    span: None,
                },
            ],
        ),
    ]));
    let evs = collect(send(&env, chat, input("what does it say?")).await.unwrap()).await;
    let c = data(&evs, "citations");
    let items = c["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], "file");
    assert_eq!(items[0]["title"], "Q3 Report.pdf");
    assert_eq!(items[0]["attachment_id"], doc.to_string());
    assert_eq!(items[0]["snippet"], "");
    assert!(!serde_json::to_string(&c).unwrap().contains("file-"));

    let req = env.llm.requests.lock()[0].clone();
    assert_eq!(
        req.tools,
        vec![ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_chat0000000000001".into()],
            max_num_results: 5
        }]
    );
    assert!(
        req.instructions
            .contains(crate::config::DEFAULT_FILE_SEARCH_GUARD)
    );
    assert_eq!(req.metadata["feature"], "file_search");
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert_eq!(t.file_search_completed_count, 1);
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(usage[0]["file_search_calls"], 1);
    env.shutdown().await;
}

#[tokio::test]
async fn incomplete_response_completes_without_citations() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(events(vec![LlmEvent::Completed(LlmCompletion {
        response_id: None,
        usage: None,
        citations: vec![RawCitation::Web {
            url: "https://a.b".into(),
            title: "t".into(),
            snippet: String::new(),
            span: None,
        }],
        output_text: "only output text".into(),
        incomplete_reason: Some("max_tokens".into()),
    })]));
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    assert_eq!(names(&evs), vec!["stream_started", "done"]);
    assert_eq!(data(&evs, "done")["usage"]["input_tokens"], 0);
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert_eq!(t.state, "completed");
    assert!(t.error_code.is_none());
    // The stored content is the accumulated delta text only (no `output_text` fallback).
    let m = messages(&env, chat).await;
    assert_eq!(m[1].content, "");
    // The provider reported no usage: the usage event carries `usage: null`.
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert!(usage[0]["usage"].is_null(), "{}", usage[0]);
    assert_eq!(usage[0]["settlement_method"], "actual");
    env.shutdown().await;
}

#[tokio::test]
async fn ping_before_first_delta() {
    let env = env_with(|o| o.cfg.streaming.sse_ping_interval_seconds = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Events {
        events: FakeLlm::default_events(),
        delay: Duration::from_millis(1300),
    });
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    let n = names(&evs);
    assert_eq!(n[0], "stream_started");
    assert_eq!(n[1], "ping");
    let first_delta = n.iter().position(|x| *x == "delta").unwrap();
    assert!(n[first_delta..].iter().all(|x| *x != "ping"), "{n:?}");
    assert_eq!(data(&evs, "ping"), serde_json::json!({}));
    env.shutdown().await;
}

#[tokio::test]
async fn lost_cas_sends_stream_interrupted() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Events {
        events: FakeLlm::default_events(),
        delay: Duration::from_millis(200),
    });
    let mut s = live(send(&env, chat, input("x")).await.unwrap())
        .into_events()
        .boxed();
    let rid = Uuid::parse_str(
        s.next().await.unwrap().data_json()["request_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    // Another finalizer (e.g. the orphan watchdog) wins the CAS.
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::State, Expr::value("failed"))
        .col_expr(
            chat_turn::Column::ErrorCode,
            Expr::value(Some("orphan_timeout".to_owned())),
        )
        .col_expr(
            chat_turn::Column::CompletedAt,
            Expr::value(Some(time::OffsetDateTime::now_utc())),
        )
        .filter(chat_turn::Column::RequestId.eq(rid))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let rest: Vec<_> = s.collect().await;
    assert_eq!(names(&rest), vec!["delta", "delta", "error"]);
    assert_eq!(data(&rest, "error")["code"], "stream_interrupted");
    // The loser wrote nothing.
    assert_eq!(messages(&env, chat).await.len(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        env.delivered_to(&env.deps.cfg.outbox.queue_name, 0)
            .await
            .is_empty()
    );
    env.shutdown().await;
}

#[tokio::test]
async fn downgrade_reported_in_done_and_replay() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-disabled").await;
    let rid = Uuid::new_v4();
    let evs = collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(rid),
                ..input("x")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    let done = data(&evs, "done");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["selected_model"], "gpt-disabled");
    assert_eq!(done["downgrade_from"], "gpt-disabled");
    assert_eq!(done["downgrade_reason"], "model_disabled");
    let audit = env
        .delivered_to(&env.deps.cfg.outbox.audit_queue_name, 1)
        .await;
    assert_eq!(
        audit[0]["policy_decisions"]["quota"]["decision"],
        "downgrade"
    );

    let evs = collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(rid),
                ..input("x")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    let done = data(&evs, "done");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "gpt-disabled");
    assert!(done.get("downgrade_reason").is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn progress_refresh_only_while_running() {
    // The CAS-guarded refresh never touches a terminal turn.
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    let t = turn(&env, chat, started_request_id(&evs)).await;
    assert!(t.last_progress_at.is_some());
    assert_eq!(t.state, "completed");
    env.shutdown().await;
}

// ── HTTP layer ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_stream_message_sse_and_problem() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let app = router(&env);
    let uri = format!("/mini-chat/v1/chats/{chat}/messages:stream");
    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            &uri,
            Some(serde_json::json!({"content": "Hi"})),
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    let evs = parse_sse(&body_bytes(resp).await);
    let n: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(n, vec!["stream_started", "delta", "delta", "done"]);
    assert_eq!(
        evs[1].1,
        serde_json::json!({"type": "text", "content": "Hello"})
    );

    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            &uri,
            Some(serde_json::json!({"content": "  "})),
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(
        body["context"]["field_violations"][0]["reason"],
        "EMPTY_CONTENT"
    );
    assert!(body.get("code").is_none());

    // 409 turn_already_running shape.
    env.llm.push(Script::Hang(vec![]));
    let running = app
        .clone()
        .oneshot(request(
            "POST",
            &uri,
            Some(serde_json::json!({"content": "A"})),
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(running.status(), 200);
    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            &uri,
            Some(serde_json::json!({"content": "B"})),
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(body["context"]["reason"], "turn_already_running");
    drop(running);

    // Non-UUID path.
    let resp = app
        .clone()
        .oneshot(request(
            "POST",
            "/mini-chat/v1/chats/not-a-uuid/messages:stream",
            Some(serde_json::json!({"content": "x"})),
            ctx_a1(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    env.shutdown().await;
}

#[test]
fn citation_mapping_rules() {
    let mut map = HashMap::new();
    let id = Uuid::new_v4();
    map.insert("file-a".to_owned(), (id, "a.pdf".to_owned()));
    let out = map_citations(
        &[
            RawCitation::File {
                file_id: "file-a".into(),
                filename: None,
                span: Some((1, 2)),
            },
            RawCitation::File {
                file_id: "file-b".into(),
                filename: Some("b".into()),
                span: None,
            },
            RawCitation::Web {
                url: "u".into(),
                title: "t".into(),
                snippet: "s".into(),
                span: None,
            },
        ],
        &map,
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].attachment_id, Some(id));
    assert!(out[0].span.is_none());
    assert_eq!(out[1].url.as_deref(), Some("u"));
    assert!(out[1].span.is_none());
}

#[test]
fn attachment_id_validation() {
    let rag = crate::config::RagConfig {
        max_documents_per_chat: 1,
        max_images_per_message: 1,
        ..crate::config::RagConfig::default()
    };
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    assert!(validate_attachment_ids(&[a, b], &rag).is_ok());
    assert!(validate_attachment_ids(&[a, b, c], &rag).is_err());
    assert!(validate_attachment_ids(&[a, a], &rag).is_err());
    assert!(validate_attachment_ids(&[], &rag).is_ok());
}

// ── Review fixes: check order, pings while opening, progress refresh ───────

#[tokio::test]
async fn idempotency_and_parallel_guard_run_before_validation() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let done = Uuid::new_v4();
    collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(done),
                ..input("a")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    let dup = Uuid::new_v4();
    // Replay of a completed turn even when the retried body would fail validation.
    let r = send(
        &env,
        chat,
        SendInput {
            request_id: Some(done),
            attachment_ids: vec![dup, dup],
            ..input("   ")
        },
    )
    .await
    .unwrap();
    assert!(matches!(r, StreamStart::Replay(_)));

    // A failed turn: 409 before EMPTY_CONTENT / INVALID_ATTACHMENT.
    env.llm.push(Script::Fail(LlmFailure {
        code: stream_codes::PROVIDER_ERROR,
        message: "boom".into(),
        usage: None,
        response_id: None,
        context_length_exceeded: false,
    }));
    let failed = Uuid::new_v4();
    collect(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(failed),
                ..input("b")
            },
        )
        .await
        .unwrap(),
    )
    .await;
    let e = send(
        &env,
        chat,
        SendInput {
            request_id: Some(failed),
            attachment_ids: vec![dup, dup],
            ..input("  ")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::REQUEST_ID_CONFLICT);

    // A running turn: the parallel guard runs before EMPTY_CONTENT.
    env.llm.push(Script::Hang(vec![]));
    let l = live(send(&env, chat, input("c")).await.unwrap());
    let e = send(&env, chat, input("  ")).await.err().unwrap();
    assert_reason(&e, reasons::TURN_ALREADY_RUNNING);
    drop(l);
    env.shutdown().await;
}

#[tokio::test]
async fn too_many_images_is_checked_before_the_quota_preflight() {
    let exhausted = mini_chat_sdk::TierLimits {
        limit_daily_credits_micro: 1,
        limit_monthly_credits_micro: 1,
    };
    let env = env_with(|o| {
        o.cfg.rag.max_images_per_message = 1;
        o.standard_limits = exhausted.clone();
        o.premium_limits = exhausted.clone();
    })
    .await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let img = |f: &'static str| Att {
        kind: "image",
        for_file_search: false,
        provider_file_id: Some(f),
        filename: "a.png",
        ..Att::default()
    };
    let i1 = add_attachment(&env, chat, TENANT_A, img("file-img00000000000011")).await;
    let i2 = add_attachment(&env, chat, TENANT_A, img("file-img00000000000012")).await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![i1, i2],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::TOO_MANY_IMAGES);
    // Within the image limit the exhausted quota answers (429).
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![i1],
            ..input("x")
        },
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(e, DomainError::ResourceExhausted { .. }),
        "{e:?}"
    );
    env.shutdown().await;
}

#[tokio::test]
async fn input_too_long_is_checked_before_the_image_guards() {
    let env = env_with(|o| {
        o.kill_switches.disable_images = true;
        let mut small = model("small-input", "premium");
        small.max_input_tokens = 200;
        o.catalog.push(small);
    })
    .await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "small-input").await;
    let i = add_attachment(
        &env,
        chat,
        TENANT_A,
        Att {
            kind: "image",
            for_file_search: false,
            provider_file_id: Some("file-img00000000000021"),
            filename: "a.png",
            ..Att::default()
        },
    )
    .await;
    let e = send(
        &env,
        chat,
        SendInput {
            attachment_ids: vec![i],
            ..input(&"x".repeat(2000))
        },
    )
    .await
    .err()
    .unwrap();
    assert_reason(&e, reasons::INPUT_TOO_LONG);
    env.shutdown().await;
}

#[tokio::test]
async fn ping_while_the_provider_stream_is_opening() {
    let env = env_with(|o| o.cfg.streaming.sse_ping_interval_seconds = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::SlowOpen {
        open_delay: Duration::from_millis(1300),
        events: FakeLlm::default_events(),
    });
    let evs = collect(send(&env, chat, input("x")).await.unwrap()).await;
    let n = names(&evs);
    assert_eq!(n[0], "stream_started");
    assert_eq!(n[1], "ping", "{n:?}");
    assert_eq!(n.last(), Some(&"done"));
    let first_delta = n.iter().position(|x| *x == "delta").unwrap();
    assert!(n[first_delta..].iter().all(|x| *x != "ping"), "{n:?}");
    env.shutdown().await;
}

#[tokio::test]
async fn progress_refresh_bumps_updated_at() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(Script::Hang(vec![LlmEvent::TextDelta("x".into())]));
    let rid = Uuid::new_v4();
    let mut l = live(
        send(
            &env,
            chat,
            SendInput {
                request_id: Some(rid),
                ..input("x")
            },
        )
        .await
        .unwrap(),
    )
    .into_events()
    .boxed();
    assert_eq!(l.next().await.unwrap().name(), "stream_started");
    assert_eq!(l.next().await.unwrap().name(), "delta");
    let before = turn(&env, chat, rid).await;
    // Age the row, then let the provider task refresh it.
    let old = before.updated_at - time::Duration::hours(1);
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(old))
        .filter(chat_turn::Column::Id.eq(before.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let task = TurnTask {
        deps: Arc::clone(&env.deps),
        quota: Arc::clone(&env.services.quota),
        turn: TurnContext {
            tenant_id: TENANT_A,
            user_id: USER_A1,
            chat_id: chat,
            turn_id: before.id,
            request_id: rid,
            selected_model: "gpt-premium".into(),
            effective_model: "gpt-premium".into(),
            downgrade_reason: None,
            periods: crate::domain::service::quota::QuotaPeriods::of(time::OffsetDateTime::now_utc()),
            started: Instant::now(),
            summary_trigger: None,
        },
        request: None,
        file_map: HashMap::new(),
        knowledge: None,
        assistant_message_id: Uuid::new_v4(),
        tx: mpsc::channel(1).0,
        cancel: CancellationToken::new(),
        text: String::new(),
        counters: ToolCounters::default(),
        web_search_started: 0,
        code_interpreter_started: 0,
        content_started: false,
        last_refresh: Instant::now(),
    };
    task.refresh_progress().await.unwrap();
    let after = turn(&env, chat, rid).await;
    assert!(after.updated_at > old, "{:?} vs {:?}", after.updated_at, old);
    drop(l);
    wait_terminal(&env, chat, rid).await;
    env.shutdown().await;
}
