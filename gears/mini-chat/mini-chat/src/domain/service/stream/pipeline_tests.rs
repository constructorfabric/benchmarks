#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned
)]
//! Slices 1 to 3 and 6: the send pipeline up to the reserve commit, the
//! hand-off to the provider task and the behaviour of a caller that is gone.
//!
//! Collaborators are fakes with call logs, so the order of the steps (check
//! order of the feature, reads before the reserve, the provider only after the
//! commit) is asserted directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use mini_chat_sdk::{KillSwitches, ModelTier};
use tokio::sync::Notify;
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::preflight::QuotaDecision;
use crate::domain::service::quota::test_support::{
    Metric, NO_SWITCHES, PolicyCall, RepoCall, design_catalog, model, snapshot, with_switches,
};
use crate::domain::service::quota::types::{BucketRow, QuotaBucket, QuotaPeriod};
use crate::domain::service::stream::ports::{HistoryRole, MessageBoundary, ThreadSummaryInfo};
use crate::domain::service::stream::test_support::{
    ASSISTANT, CHAT, REQUEST, StreamMetric, TurnsCall, World, at, chat, completed, ctx, doc,
    history, image, other_running, text, turn,
};
use crate::domain::service::stream::types::{DeltaKind, StreamEvent, TurnState};
use crate::test_doubles::{LogCapture, PdpReply, USER, USER_TENANT};

fn req(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: Some(REQUEST),
        ..SendRequest::default()
    }
}

async fn send(world: &World, request: SendRequest) -> Result<SendOutcome, DomainError> {
    world.service.send(ctx(), CHAT, request).await
}

fn refusal(result: Result<SendOutcome, DomainError>) -> DomainError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("the send was expected to be refused"),
    }
}

fn opened(result: Result<SendOutcome, DomainError>) -> (StreamStarted, RelayHandle) {
    match result {
        Ok(SendOutcome::Stream { started, relay }) => (started, relay),
        Ok(SendOutcome::Replay(_)) => panic!("a new stream expected, got a replay"),
        Err(error) => panic!("a new stream expected, got {error:?}"),
    }
}

/// Lets the spawned tasks run.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

async fn wait_for_finalization(world: &World) {
    for _ in 0..1000 {
        if !world.turns.finalizations().is_empty() {
            return;
        }
        settle().await;
    }
    panic!("the turn was never finalized");
}

/// The gauge guard drops when the task returns, after the terminal commit.
async fn wait_for_idle_gauge(world: &World) {
    for _ in 0..1000 {
        if world.metrics.active() == 0 {
            return;
        }
        settle().await;
    }
    panic!("mini_chat_active_streams never went back to 0");
}

fn assert_no_side_effects(world: &World) {
    assert_eq!(world.turns.creates().len(), 0, "no reserve");
    assert_eq!(world.provider.open_count(), 0, "no provider call");
    assert_eq!(world.provider.builds().len(), 0);
}

// ---- the check order before authorization ------------------------------------------------------

#[tokio::test]
async fn empty_content_is_refused_before_any_pdp_call_or_read() {
    let world = World::new();
    let error = refusal(send(&world, req("  \n")).await);
    assert!(matches!(error, DomainError::EmptyContent), "{error:?}");
    assert!(world.pdp.requests().is_empty());
    assert!(world.chats.loads.lock().unwrap().is_empty());
    assert!(world.turns.calls().is_empty());
}

#[tokio::test]
async fn duplicate_attachment_ids_are_refused_before_authorization() {
    let world = World::new();
    let mut request = req("hi");
    request.attachment_ids = vec![Uuid::from_u128(5), Uuid::from_u128(5)];
    let error = refusal(send(&world, request).await);
    assert!(
        matches!(error, DomainError::InvalidAttachmentIds),
        "{error:?}"
    );
    assert!(world.pdp.requests().is_empty());
}

#[tokio::test]
async fn too_many_attachment_ids_are_refused_before_authorization() {
    let world = World::new();
    let mut request = req("hi");
    request.attachment_ids = (1..=60).map(Uuid::from_u128).collect();
    let error = refusal(send(&world, request).await);
    assert!(matches!(error, DomainError::InvalidAttachmentIds));
    assert!(world.pdp.requests().is_empty());
}

#[tokio::test]
async fn empty_content_wins_on_a_chat_the_caller_cannot_see() {
    let world = World::build(None, PdpReply::Deny, super::super::test_support::settings());
    let error = refusal(send(&world, req("")).await);
    assert!(matches!(error, DomainError::EmptyContent), "{error:?}");
}

// ---- authorization and the chat -----------------------------------------------------------------

#[tokio::test]
async fn the_send_is_authorized_as_send_message_on_the_chat_and_read_is_not_evaluated() {
    let world = World::new();
    let _ = opened(send(&world, req("hi")).await);
    let requests = world.pdp.requests();
    assert_eq!(requests.len(), 1, "one evaluation");
    assert_eq!(requests[0].action.name, "send_message");
    assert_eq!(requests[0].resource.id, Some(CHAT));
}

#[tokio::test]
async fn a_pdp_deny_is_authz_denied_and_nothing_else_happens() {
    let world = World::build(
        Some(chat("p")),
        PdpReply::Deny,
        super::super::test_support::settings(),
    );
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
    assert!(world.chats.loads.lock().unwrap().is_empty());
    assert!(world.turns.calls().is_empty());
    assert!(world.policy.calls().is_empty());
}

#[tokio::test]
async fn a_pdp_failure_is_pdp_unavailable() {
    let world = World::build(
        Some(chat("p")),
        PdpReply::Failure,
        super::super::test_support::settings(),
    );
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::PdpUnavailable), "{error:?}");
    assert!(world.turns.calls().is_empty());
}

#[tokio::test]
async fn a_missing_chat_is_not_found_before_any_turn_read() {
    let world = World::build(
        None,
        PdpReply::AllowScoped,
        super::super::test_support::settings(),
    );
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(
        error,
        DomainError::NotFound {
            resource: crate::domain::error::ResourceKind::Chat
        }
    ));
    assert!(world.turns.calls().is_empty());
}

// ---- idempotency and the guard ------------------------------------------------------------------

#[tokio::test]
async fn a_completed_request_id_is_replayed_without_any_other_step() {
    let world = World::new();
    world.turns.with(|t| {
        t.by_request.push(turn(TurnState::Completed));
        t.assistant = Some(super::super::ports::StoredAssistantMessage {
            id: ASSISTANT,
            content: "stored answer".to_owned(),
            input_tokens: 10,
            output_tokens: 5,
        });
        // Another turn is running: the replay is still served.
        t.running = Some(other_running());
    });
    let SendOutcome::Replay(events) = send(&world, req("same text")).await.unwrap() else {
        panic!("a replay expected");
    };
    let names: Vec<_> = events.iter().map(StreamEvent::name).collect();
    assert_eq!(names, ["stream_started", "delta", "done"]);
    assert_eq!(
        events[1],
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: "stored answer".to_owned(),
        }
    );
    assert!(
        world.policy.calls().is_empty(),
        "no catalog check for a replay"
    );
    assert!(world.quota_repo.calls().is_empty(), "no quota step");
    assert_no_side_effects(&world);
    assert!(
        world.metrics.events().is_empty(),
        "replay records no metric"
    );
    assert_eq!(world.turns.finalizations().len(), 0);
}

#[tokio::test]
async fn replay_works_for_a_chat_whose_model_left_the_catalog() {
    let world = World::build(
        Some(chat("removed-model")),
        PdpReply::AllowScoped,
        super::super::test_support::settings(),
    );
    world.turns.with(|t| {
        t.by_request.push(turn(TurnState::Completed));
        t.assistant = Some(super::super::ports::StoredAssistantMessage {
            id: ASSISTANT,
            content: String::new(),
            input_tokens: 0,
            output_tokens: 0,
        });
    });
    assert!(matches!(
        send(&world, req("x")).await.unwrap(),
        SendOutcome::Replay(_)
    ));
}

#[tokio::test]
async fn a_replay_of_a_message_less_turn_is_internal() {
    let world = World::new();
    world
        .turns
        .with(|t| t.by_request.push(turn(TurnState::Completed)));
    let error = refusal(send(&world, req("x")).await);
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
}

#[tokio::test]
async fn running_failed_cancelled_and_deleted_turns_conflict_on_the_request_id() {
    for (state, deleted) in [
        (TurnState::Running, false),
        (TurnState::Failed, false),
        (TurnState::Cancelled, false),
        (TurnState::Completed, true),
    ] {
        let world = World::new();
        let mut row = turn(state);
        row.deleted = deleted;
        world.turns.with(|t| t.by_request.push(row));
        let error = refusal(send(&world, req("x")).await);
        assert!(
            matches!(error, DomainError::RequestIdConflict),
            "{state:?} deleted={deleted}: {error:?}"
        );
        assert!(world.policy.calls().is_empty());
        assert_no_side_effects(&world);
    }
}

#[tokio::test]
async fn another_running_turn_is_turn_already_running_before_any_quota_step() {
    let world = World::new();
    world.turns.with(|t| t.running = Some(other_running()));
    let error = refusal(send(&world, req("x")).await);
    assert!(
        matches!(error, DomainError::TurnAlreadyRunning),
        "{error:?}"
    );
    assert!(world.policy.calls().is_empty());
    assert!(world.quota_repo.calls().is_empty());
    assert_no_side_effects(&world);
}

// ---- the chat model check ------------------------------------------------------------------------

#[tokio::test]
async fn the_catalog_is_read_fresh_by_version_then_snapshot() {
    let world = World::new();
    let _ = opened(send(&world, req("hi")).await);
    let calls = world.policy.calls();
    assert_eq!(calls[0], PolicyCall::Version(USER));
    assert_eq!(calls[1], PolicyCall::Snapshot(USER, 7));
}

#[tokio::test]
async fn a_chat_model_missing_from_the_catalog_is_invalid_model_before_any_quota_step() {
    let world = World::build(
        Some(chat("removed-model")),
        PdpReply::AllowScoped,
        super::super::test_support::settings(),
    );
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::InvalidModel), "{error:?}");
    assert!(
        world.quota_repo.calls().is_empty(),
        "the preflight did not run"
    );
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn a_disabled_chat_model_proceeds_and_is_downgraded() {
    let catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("s", ModelTier::Standard).build(),
    ];
    let world = World::new().with_catalog(catalog);
    let _ = opened(send(&world, req("hi")).await);
    assert_eq!(world.turns.creates()[0].effective_model, "s");
}

#[tokio::test]
async fn a_policy_failure_is_a_resolution_error_and_nothing_is_reserved() {
    use crate::domain::service::quota::test_support::PolicyStep;
    for step in [PolicyStep::Version, PolicyStep::Snapshot] {
        let world = World::new();
        world.policy.fail(step);
        let error = refusal(send(&world, req("hi")).await);
        assert!(
            matches!(error, DomainError::PolicyResolution(_)),
            "{step:?}: {error:?}"
        );
        assert_no_side_effects(&world);
    }
}

// ---- turn inputs and the preflight --------------------------------------------------------------

#[tokio::test]
async fn too_many_images_is_refused_before_the_preflight() {
    let world = World::new();
    let rows: Vec<_> = (1..=5).map(image).collect();
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    world.turns.with(|t| t.request_attachments = rows);
    let mut request = req("look");
    request.attachment_ids = ids;
    let error = refusal(send(&world, request).await);
    assert!(matches!(error, DomainError::TooManyImages), "{error:?}");
    assert!(world.quota_repo.calls().is_empty(), "before any quota step");
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn the_preflight_gets_the_tenant_the_user_the_chat_and_the_selected_model() {
    let world = World::new();
    let _ = opened(send(&world, req("hi")).await);
    let RepoCall::ReadPreflight(read) = world.quota_repo.calls()[0].clone() else {
        panic!("the preflight read comes first");
    };
    assert_eq!(
        (read.tenant_id, read.user_id, read.chat_id),
        (USER_TENANT, USER, CHAT)
    );
}

#[tokio::test]
async fn an_exhausted_quota_is_refused_before_anything_is_reserved() {
    let world = World::new();
    *world.quota_repo.rows.lock().unwrap() = crate::domain::service::quota::ports::PreflightRows {
        prior_context_tokens: 0,
        buckets: vec![BucketRow::new(
            QuotaPeriod::Daily,
            QuotaBucket::Total,
            60_000_000,
            0,
        )],
    };
    let error = refusal(send(&world, req("hi")).await);
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded(crate::domain::error::QuotaSubject::Tokens)
        ),
        "{error:?}"
    );
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn web_search_under_its_kill_switch_is_refused() {
    let world = World::new().with_snapshot(with_switches(
        snapshot(7, design_catalog()),
        KillSwitches {
            disable_web_search: true,
            ..NO_SWITCHES
        },
    ));
    let mut request = req("hi");
    request.web_search_enabled = Some(true);
    let error = refusal(send(&world, request).await);
    assert!(matches!(
        error,
        DomainError::FeatureDisabled(crate::domain::error::DisabledFeature::WebSearch)
    ));
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn an_input_above_max_input_tokens_is_refused_after_the_preflight_and_before_the_reserve() {
    let mut small = model("p", ModelTier::Premium).build();
    small.max_input_tokens = 10;
    let world = World::new().with_catalog(vec![small, model("s", ModelTier::Standard).build()]);
    let error = refusal(send(&world, req(&"x".repeat(20))).await);
    assert!(matches!(error, DomainError::InputTooLong), "{error:?}");
    assert!(
        world
            .quota_repo
            .calls()
            .iter()
            .any(|c| matches!(c, RepoCall::ReadPreflight(_))),
        "the preflight ran first"
    );
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn images_under_their_kill_switch_are_refused_not_dropped() {
    let world = World::new().with_snapshot(with_switches(
        snapshot(7, design_catalog()),
        KillSwitches {
            disable_images: true,
            ..NO_SWITCHES
        },
    ));
    let img = image(1);
    world
        .turns
        .with(|t| t.request_attachments = vec![img.clone()]);
    let mut request = req("look");
    request.attachment_ids = vec![img.id];
    let error = refusal(send(&world, request).await);
    assert!(matches!(
        error,
        DomainError::FeatureDisabled(crate::domain::error::DisabledFeature::Images)
    ));
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn images_on_a_model_without_vision_are_refused() {
    let world = World::new().with_catalog(vec![
        model("p", ModelTier::Premium).vision(false).build(),
        model("s", ModelTier::Standard).build(),
    ]);
    let img = image(1);
    world
        .turns
        .with(|t| t.request_attachments = vec![img.clone()]);
    let mut request = req("look");
    request.attachment_ids = vec![img.id];
    let error = refusal(send(&world, request).await);
    assert!(
        matches!(error, DomainError::VisionNotSupported),
        "{error:?}"
    );
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn images_after_a_downgrade_to_a_model_without_vision_are_refused() {
    // The selected model has vision but is disabled; the standard model that
    // runs instead has none.
    let world = World::new().with_catalog(vec![
        model("p", ModelTier::Premium)
            .enabled(false)
            .vision(true)
            .build(),
        model("s", ModelTier::Standard).vision(false).build(),
    ]);
    let img = image(1);
    world
        .turns
        .with(|t| t.request_attachments = vec![img.clone()]);
    let mut request = req("look");
    request.attachment_ids = vec![img.id];
    let error = refusal(send(&world, request).await);
    assert!(
        matches!(error, DomainError::VisionNotSupported),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_context_budget_is_checked_against_the_effective_model() {
    let mut tiny = model("p", ModelTier::Premium).build();
    tiny.context_window = 400; // below max_output_tokens_applied (500)
    let world = World::new().with_catalog(vec![tiny, model("s", ModelTier::Standard).build()]);
    let error = refusal(send(&world, req("hi")).await);
    assert!(
        matches!(error, DomainError::ContextBudgetExceeded),
        "{error:?}"
    );
    assert_no_side_effects(&world);
}

// ---- the provider request -----------------------------------------------------------------------

#[tokio::test]
async fn a_provider_that_cannot_be_resolved_is_an_internal_refusal_before_the_reserve() {
    let world = World::new();
    world.directory.fail.store(true, Ordering::SeqCst);
    let error = refusal(send(&world, req("hi")).await);
    assert!(
        matches!(error, DomainError::ProviderResolution(_)),
        "{error:?}"
    );
    assert_no_side_effects(&world);
}

#[tokio::test]
async fn a_request_that_cannot_be_built_is_an_internal_refusal_before_the_reserve() {
    let world = World::new();
    world.provider.fail_build("body cannot be serialized");
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    assert_eq!(world.turns.creates().len(), 0);
}

#[tokio::test]
async fn the_provider_is_resolved_for_the_effective_models_provider_and_the_chat_tenant() {
    let world = World::new();
    let _ = opened(send(&world, req("hi")).await);
    assert_eq!(
        world.directory.resolved.lock().unwrap().clone(),
        vec![("openai".to_owned(), USER_TENANT)]
    );
}

#[tokio::test]
async fn the_request_is_built_from_the_plan_the_tools_and_the_applied_output_limit() {
    let world = World::new();
    world.turns.with(|t| {
        t.ready = vec![doc(1)];
        t.vector_store = Some("vs_chat".to_owned());
    });
    let mut request = req("hello");
    request.web_search_enabled = Some(true);
    let _ = opened(send(&world, request).await);
    let build = world.provider.builds().remove(0);
    assert_eq!(build.model, "provider-p");
    assert_eq!(build.target.alias, "api.openai.test");
    assert_eq!(build.max_output_tokens_applied, 500);
    let tools: Vec<_> = build.tools.tools.iter().map(|t| t.name()).collect();
    assert_eq!(tools, ["file_search", "web_search"]);
    assert!(build.plan.instructions.starts_with("SECRET-SYSTEM-PROMPT"));
    assert!(build.plan.instructions.ends_with("FS-GUARD\n\nWS-GUARD"));
    assert_eq!(build.plan.current.unwrap().content, "hello");
    assert_eq!(
        (build.tenant_id, build.user_id, build.chat_id),
        (USER_TENANT, USER, CHAT)
    );
}

#[tokio::test]
async fn the_history_and_the_summary_reach_the_plan_and_stream_started() {
    let world = World::new();
    let boundary = MessageBoundary {
        created_at: at(-10),
        id: Uuid::from_u128(0x4009),
    };
    world.turns.with(|t| {
        t.boundary = Some(boundary);
        t.summary = Some(ThreadSummaryInfo {
            summary_text: "earlier talk".to_owned(),
            frontier: MessageBoundary {
                created_at: at(-4000),
                id: Uuid::from_u128(0x4000),
            },
            token_estimate: 33,
        });
        t.recent = vec![
            history(1, HistoryRole::User, "q", 1),
            history(2, HistoryRole::Assistant, "a", 2),
        ];
    });
    let (started, _relay) = opened(send(&world, req("next")).await);
    assert_eq!(started.thread_summary_applied, Some(33));
    let plan = world.provider.builds().remove(0).plan;
    assert!(plan.summary.is_some());
    assert_eq!(plan.history.len(), 2);
}

#[tokio::test]
async fn stream_started_has_no_summary_marker_without_a_kept_summary() {
    let world = World::new();
    let (started, _relay) = opened(send(&world, req("hi")).await);
    assert_eq!(started.thread_summary_applied, None);
}

// ---- the reserve --------------------------------------------------------------------------------

#[tokio::test]
async fn the_reserve_creates_the_turn_with_pregenerated_ids_and_the_request_data() {
    let world = World::new();
    let mut request = req("  exact text ");
    request.web_search_enabled = Some(true);
    let (started, _relay) = opened(send(&world, request).await);
    let creates = world.turns.creates();
    assert_eq!(creates.len(), 1);
    let create = &creates[0];
    assert_eq!(create.content, "  exact text ");
    assert_eq!(create.request_id, REQUEST);
    assert_eq!(create.chat_id, CHAT);
    assert_eq!((create.tenant_id, create.user_id), (USER_TENANT, USER));
    assert!(create.web_search_enabled);
    assert!(create.send_mode);
    assert_eq!(create.now, at(0));
    assert_eq!(create.effective_model, "p");
    for id in [create.turn_id, create.user_message_id, started.message_id] {
        assert_eq!(id.get_version_num(), 4);
    }
    let mut distinct = vec![create.turn_id, create.user_message_id, started.message_id];
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "three different ids");
    assert_eq!(started.request_id, REQUEST);
    assert!(started.is_new_turn);
}

#[tokio::test]
async fn an_absent_request_id_is_generated_once_and_used_everywhere() {
    let world = World::new();
    let mut request = req("hi");
    request.request_id = None;
    let (started, _relay) = opened(send(&world, request).await);
    assert_eq!(started.request_id.get_version_num(), 4);
    assert_eq!(world.turns.creates()[0].request_id, started.request_id);
}

#[tokio::test]
async fn the_attachment_ids_are_passed_to_the_reserve_in_request_order() {
    let world = World::new();
    let rows = vec![image(2), image(1), doc(1)];
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    world.turns.with(|t| t.request_attachments = rows);
    let mut request = req("files");
    request.attachment_ids = ids.clone();
    let _ = opened(send(&world, request).await);
    assert_eq!(world.turns.creates()[0].attachment_ids, ids);
}

#[tokio::test]
async fn every_read_runs_before_the_reserve_and_the_provider_only_after_it() {
    let world = World::new();
    let provider = Arc::clone(&world.provider);
    let calls_at_commit = Arc::new(AtomicUsize::new(usize::MAX));
    let seen = Arc::clone(&calls_at_commit);
    *world.turns.gates.after_create.lock().unwrap() = Some(Arc::new(move || {
        let provider = Arc::clone(&provider);
        let seen = Arc::clone(&seen);
        Box::pin(async move {
            seen.store(
                provider.open_count() + provider.builds().len() * 1000,
                Ordering::SeqCst,
            );
        })
    }));
    let _ = opened(send(&world, req("hi")).await);
    // The request is built (1000) before the reserve but not sent (0).
    assert_eq!(calls_at_commit.load(Ordering::SeqCst), 1000);
    let calls = world.turns.calls();
    let create_at = calls
        .iter()
        .position(|c| *c == TurnsCall::CreateTurn)
        .unwrap();
    for (position, call) in calls.iter().enumerate() {
        let is_read = matches!(
            call,
            TurnsCall::FindByRequestId(..)
                | TurnsCall::FindRunning(_)
                | TurnsCall::RequestAttachments(_)
                | TurnsCall::ReadyAttachments
                | TurnsCall::VectorStore
                | TurnsCall::Boundary
                | TurnsCall::Summary
                | TurnsCall::Recent(_)
        );
        if is_read {
            assert!(position < create_at, "{call:?} runs after the reserve");
        }
    }
}

#[tokio::test]
async fn a_quota_refusal_at_the_recheck_is_propagated_and_nothing_is_spawned() {
    let world = World::new();
    world.turns.with(|t| {
        t.create_results.push_back(Err(DomainError::QuotaExceeded(
            crate::domain::error::QuotaSubject::Tokens,
        )));
    });
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(
        error,
        DomainError::QuotaExceeded(crate::domain::error::QuotaSubject::Tokens)
    ));
    settle().await;
    assert_eq!(world.provider.open_count(), 0);
    assert_eq!(world.metrics.events().len(), 0, "no stream metric");
}

#[tokio::test]
async fn an_attachment_refused_in_the_transaction_is_invalid_attachment() {
    let world = World::new();
    world.turns.with(|t| {
        t.create_results
            .push_back(Err(DomainError::InvalidAttachmentIds))
    });
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::InvalidAttachmentIds));
    assert_eq!(world.provider.open_count(), 0);
}

#[tokio::test]
async fn a_failed_reserve_is_a_database_error_and_nothing_follows() {
    let world = World::new();
    world.turns.with(|t| {
        t.create_results
            .push_back(Err(DomainError::Database("conflicts".to_owned())))
    });
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::Database(_)));
    settle().await;
    assert_eq!(world.provider.open_count(), 0);
    assert!(world.turns.finalizations().is_empty());
}

// ---- the insert race ----------------------------------------------------------------------------

fn unique_violation(world: &World) {
    world.turns.with(|t| {
        t.create_results
            .push_back(Err(DomainError::UniqueViolation))
    });
}

#[tokio::test]
async fn a_race_lost_to_a_completed_turn_with_the_same_request_id_is_a_replay() {
    let world = World::new();
    unique_violation(&world);
    world.turns.with(|t| {
        t.race_winner = Some(turn(TurnState::Completed));
        t.assistant = Some(super::super::ports::StoredAssistantMessage {
            id: ASSISTANT,
            content: "winner's answer".to_owned(),
            input_tokens: 1,
            output_tokens: 2,
        });
    });
    let SendOutcome::Replay(events) = send(&world, req("hi")).await.unwrap() else {
        panic!("a replay expected");
    };
    assert_eq!(events.len(), 3);
    assert_eq!(world.provider.open_count(), 0);
}

#[tokio::test]
async fn a_race_lost_to_the_same_request_id_still_running_is_a_request_id_conflict() {
    let world = World::new();
    unique_violation(&world);
    world
        .turns
        .with(|t| t.race_winner = Some(turn(TurnState::Running)));
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::RequestIdConflict), "{error:?}");
}

#[tokio::test]
async fn a_race_lost_to_a_different_request_id_is_turn_already_running() {
    let world = World::new();
    unique_violation(&world);
    world.turns.with(|t| t.race_running = Some(other_running()));
    let error = refusal(send(&world, req("hi")).await);
    assert!(
        matches!(error, DomainError::TurnAlreadyRunning),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_violation_without_a_conflicting_row_is_internal_and_logged() {
    let logs = LogCapture::start();
    let world = World::new();
    unique_violation(&world);
    let error = refusal(send(&world, req("hi")).await);
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    assert!(!logs.at(tracing::Level::ERROR).is_empty());
}

#[tokio::test]
async fn a_unique_violation_never_reaches_the_caller_as_already_exists() {
    for winner in [Some(turn(TurnState::Failed)), None] {
        let world = World::new();
        unique_violation(&world);
        world.turns.with(|t| {
            t.race_winner = winner;
            t.race_running = Some(other_running());
        });
        let error = refusal(send(&world, req("hi")).await);
        assert!(!matches!(error, DomainError::UniqueViolation), "{error:?}");
    }
}

// ---- the hand-off -------------------------------------------------------------------------------

#[tokio::test]
async fn the_reserve_metrics_and_the_image_count_are_recorded_after_the_commit() {
    let world = World::new();
    let rows = vec![image(1), image(2)];
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    world.turns.with(|t| t.request_attachments = rows);
    let mut request = req("two images");
    request.attachment_ids = ids;
    let _ = opened(send(&world, request).await);
    let reserves = world
        .quota_metrics
        .events()
        .into_iter()
        .filter(|m| matches!(m, Metric::Reserve(_)))
        .count();
    assert_eq!(reserves, 2, "once per period");
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::ImageInputs(2)),
        1
    );
}

#[tokio::test]
async fn a_text_only_turn_records_zero_image_inputs() {
    let world = World::new();
    let _ = opened(send(&world, req("hi")).await);
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::ImageInputs(0)),
        1
    );
}

#[tokio::test]
async fn a_refused_send_records_no_reserve_and_no_image_metric() {
    let world = World::new();
    world.turns.with(|t| t.running = Some(other_running()));
    let _ = refusal(send(&world, req("hi")).await);
    assert!(world.quota_metrics.events().is_empty());
    assert!(world.metrics.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_whole_turn_streams_from_the_reserve_to_done() {
    let world = World::new();
    let feed = world.provider.expect_stream();
    let (started, relay) = opened(send(&world, req("hi")).await);
    let mut stream = relay.into_stream(started.clone());
    feed.push_all([text("Hel"), text("lo"), completed(11, 7)]);
    let mut names = Vec::new();
    let mut last = None;
    while let Some(event) = stream.next().await {
        names.push(event.name());
        last = Some(event);
    }
    assert_eq!(names, ["stream_started", "delta", "delta", "done"]);
    let Some(StreamEvent::Done(done)) = last else {
        panic!("done expected");
    };
    assert_eq!(done.usage, (11, 7));
    assert_eq!(done.quota_decision, QuotaDecision::Allow);
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(
        request.columns.assistant_message_id,
        Some(started.message_id)
    );
    let message = writes.assistant_message.unwrap();
    assert_eq!(message.id, started.message_id);
    assert_eq!(message.content, "Hello");
    assert_eq!(world.provider.open_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn pings_come_every_interval_while_the_provider_is_silent() {
    let world = World::new();
    let _feed = world.provider.expect_stream();
    let (started, relay) = opened(send(&world, req("hi")).await);
    let mut stream = relay.into_stream(started);
    assert_eq!(stream.next().await.unwrap().name(), "stream_started");
    assert_eq!(stream.next().await, Some(StreamEvent::Ping));
    assert_eq!(stream.next().await, Some(StreamEvent::Ping));
}

#[tokio::test(start_paused = true)]
async fn the_setup_returns_without_waiting_for_the_provider() {
    let world = World::new();
    let _feed = world.provider.expect_stream(); // never answers
    let result = tokio::time::timeout(Duration::from_secs(1), send(&world, req("hi"))).await;
    assert!(result.is_ok(), "the setup must not wait for the provider");
}

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_before_reading_cancels_the_committed_turn() {
    let world = World::new();
    let feed = world.provider.expect_stream();
    let (started, relay) = opened(send(&world, req("hi")).await);
    drop(relay.into_stream(started));
    wait_for_finalization(&world).await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(
        request.columns.state,
        mini_chat_sdk::TerminalState::Cancelled
    );
    assert_eq!(writes.assistant_message, None);
    assert!(
        world.provider.open_count() == 0 || feed.is_dropped(),
        "the provider is not left running"
    );
    wait_for_idle_gauge(&world).await;
    assert_eq!(world.metrics.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_mid_stream_cancels_the_turn_with_the_partial_text() {
    let world = World::new();
    let feed = world.provider.expect_stream();
    let (started, relay) = opened(send(&world, req("hi")).await);
    let mut stream = relay.into_stream(started);
    feed.push(text("partial"));
    stream.next().await; // stream_started
    assert_eq!(stream.next().await.unwrap().name(), "delta");
    drop(stream);
    wait_for_finalization(&world).await;
    within_dropped(&feed).await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(
        request.columns.state,
        mini_chat_sdk::TerminalState::Cancelled
    );
    assert_eq!(writes.assistant_message.unwrap().content, "partial");
    assert_eq!(
        world.metrics.count(|m| matches!(
            m,
            StreamMetric::Disconnected(
                crate::domain::service::stream::ports::DisconnectStage::MidStream
            )
        )),
        1
    );
}

async fn within_dropped(feed: &super::super::test_support::ProviderFeed) {
    tokio::time::timeout(Duration::from_secs(5), feed.dropped())
        .await
        .expect("the provider stream was not dropped");
}

// ---- a caller that is gone ----------------------------------------------------------------------

/// Drops the `send` future right after the reserve committed and before the
/// setup result is delivered; the setup task goes on.
async fn send_and_drop_after_reserve(world: &World) {
    let committed = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let committed = Arc::clone(&committed);
        let proceed = Arc::clone(&proceed);
        *world.turns.gates.after_create.lock().unwrap() = Some(Arc::new(move || {
            let committed = Arc::clone(&committed);
            let proceed = Arc::clone(&proceed);
            Box::pin(async move {
                committed.notify_one();
                proceed.notified().await;
            })
        }));
    }
    let service = Arc::clone(&world.service);
    let mut future =
        Box::pin(async move { service.send(ctx(), CHAT, req("hi")).await.map(|_| ()) });
    tokio::select! {
        _ = &mut future => panic!("the send cannot complete while the reserve hook waits"),
        () = committed.notified() => {}
    }
    drop(future); // the handler is gone
    proceed.notify_one();
}

#[tokio::test(start_paused = true)]
async fn a_caller_gone_after_the_reserve_still_gets_its_turn_finalized_as_cancelled() {
    let world = World::new();
    let _feed = world.provider.expect_stream();
    send_and_drop_after_reserve(&world).await;
    wait_for_finalization(&world).await;
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(
        request.columns.state,
        mini_chat_sdk::TerminalState::Cancelled
    );
    assert_eq!(request.columns.assistant_message_id, None);
    assert_eq!(writes.assistant_message, None, "no assistant message");
    assert_eq!(
        writes.audit.event_type,
        mini_chat_sdk::TurnAuditEventType::TurnFailed,
        "exactly one audit event of a failed turn"
    );
    assert_eq!(world.turns.finalizations().len(), 1, "one finalization");
}

#[tokio::test(start_paused = true)]
async fn a_caller_gone_after_the_reserve_never_reaches_the_provider() {
    let world = World::new();
    let _feed = world.provider.expect_stream();
    send_and_drop_after_reserve(&world).await;
    wait_for_finalization(&world).await;
    settle().await;
    assert_eq!(
        world.provider.open_count(),
        0,
        "the go-signal saw the dropped handle"
    );
}

#[tokio::test(start_paused = true)]
async fn a_caller_gone_after_the_reserve_leaves_the_gauge_and_the_disconnect_metrics_right() {
    let world = World::new();
    let _feed = world.provider.expect_stream();
    send_and_drop_after_reserve(&world).await;
    wait_for_finalization(&world).await;
    settle().await;
    wait_for_idle_gauge(&world).await;
    assert_eq!(
        world.metrics.active(),
        0,
        "mini_chat_active_streams is back"
    );
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelRequested),
        1
    );
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelEffective),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn the_setup_task_survives_a_caller_that_is_dropped_while_the_reserve_runs() {
    // The same drop point as above seen from the other side: the committed
    // turn is not stranded `running`; nothing is left without a finalizer.
    let world = World::new();
    let _feed = world.provider.expect_stream();
    send_and_drop_after_reserve(&world).await;
    wait_for_finalization(&world).await;
    assert_eq!(world.turns.creates().len(), 1);
    assert_eq!(world.turns.finalizations().len(), 1);
}
