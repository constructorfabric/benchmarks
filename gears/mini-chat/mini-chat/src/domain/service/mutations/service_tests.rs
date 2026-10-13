#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::let_underscore_must_use,
    clippy::type_complexity,
    clippy::unused_async
)]
//! The mutation service over fakes with call logs: the check order of the
//! feature, the single PDP evaluation, the pre-commit refusals that leave the
//! previous turn alone, the replacement and delete plans, the conflict
//! mapping, the post-commit setup failures and their marks, the hand-off to the
//! provider task and the mutation metrics recorded once per request.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mini_chat_sdk::{
    KillSwitches, ModelTier, TerminalState, TurnAuditEventType, TurnMutationAuditEvent,
};
use tokio::sync::Notify;
use tracing::Level;
use uuid::Uuid;

use super::*;
use crate::domain::error::{DisabledFeature, QuotaSubject, ResourceKind};
use crate::domain::service::mutations::test_support::{
    MutationCall, MutationWorld, ORIGINAL, TARGET_USER_MESSAGE, latest_of, other_turn, target,
    target_message,
};
use crate::domain::service::mutations::types::{LatestTurn, MutationOp, MutationResult};
use crate::domain::service::quota::reserve::{ReserveMode, ReservedColumns};
use crate::domain::service::quota::test_support::{
    NO_SWITCHES, PolicyStep, design_catalog, model, premium_decision, snapshot, with_switches,
};
use crate::domain::service::stream::ports::{HistoryRole, MessageBoundary, ThreadSummaryInfo};
use crate::domain::service::stream::relay::RelayHandle;
use crate::domain::service::stream::test_support::{
    CHAT, REQUEST, TURN, TurnsCall, at, chat, completed, ctx, history, image, other_running, text,
};
use crate::domain::service::stream::types::{StreamStarted, TurnState};
use crate::test_doubles::{LogCapture, OTHER_USER, PdpReply, USER, USER_TENANT};

// ---- helpers ------------------------------------------------------------------------------------

async fn retry(world: &MutationWorld) -> Result<SendOutcome, DomainError> {
    world.service.retry(ctx(), CHAT, REQUEST).await
}

async fn edit(world: &MutationWorld, content: &str) -> Result<SendOutcome, DomainError> {
    world
        .service
        .edit(ctx(), CHAT, REQUEST, content.to_owned())
        .await
}

async fn delete(world: &MutationWorld) -> Result<(), DomainError> {
    world.service.delete(&ctx(), CHAT, REQUEST).await
}

fn refusal<T>(result: Result<T, DomainError>) -> DomainError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("a refusal was expected, got a success"),
    }
}

fn opened(result: Result<SendOutcome, DomainError>) -> (StreamStarted, RelayHandle) {
    match result {
        Ok(SendOutcome::Stream { started, relay }) => (started, relay),
        Ok(SendOutcome::Replay(_)) => panic!("a new stream expected, got a replay"),
        Err(error) => panic!("a new stream expected, got {error:?}"),
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_finalization(world: &MutationWorld) {
    for _ in 0..1000 {
        if !world.turns.finalizations().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the turn was never finalized");
}

/// The previous turn, the user and the quota were not touched and the provider
/// heard nothing.
fn assert_untouched(world: &MutationWorld) {
    assert_eq!(
        world.mutations.count(|call| matches!(
            call,
            MutationCall::Replace(_) | MutationCall::Delete(_) | MutationCall::Mark { .. }
        )),
        0,
        "no write of the mutation store"
    );
    assert!(world.quota_store.reserves().is_empty(), "no reserve");
    assert_eq!(world.provider.open_count(), 0, "no provider call");
    assert!(world.provider.builds().is_empty(), "no request was built");
    assert!(world.turns.creates().is_empty());
    assert!(world.turns.finalizations().is_empty());
}

/// The replacement transaction failed: nothing of the post-commit setup ran and
/// no new turn was marked.
fn assert_nothing_after_the_transaction(world: &MutationWorld) {
    assert!(
        world.mutations.marks().is_empty(),
        "no mark: no new turn exists"
    );
    assert_eq!(
        world
            .mutations
            .count(|call| matches!(call, MutationCall::Boundary(..))),
        0,
        "no history read"
    );
    assert!(world.quota_store.reserves().is_empty(), "no reserve");
    assert_eq!(world.provider.open_count(), 0, "no provider call");
    assert!(world.provider.builds().is_empty(), "no request was built");
    assert!(world.turns.creates().is_empty());
    assert!(world.turns.finalizations().is_empty());
}

fn switches(edit: impl FnOnce(&mut KillSwitches)) -> KillSwitches {
    let mut switches = NO_SWITCHES;
    edit(&mut switches);
    switches
}

fn world_with_switches(switches: KillSwitches) -> MutationWorld {
    let world = MutationWorld::new();
    world
        .policy
        .snapshots
        .lock()
        .unwrap()
        .insert(7, with_switches(snapshot(7, design_catalog()), switches));
    world
}

fn result_of(world: &MutationWorld) -> Vec<(MutationOp, MutationResult)> {
    world.metrics.results()
}

// ==== slice 1: delete ===============================================================================

#[tokio::test]
async fn a_delete_of_the_latest_terminal_turn_succeeds_with_its_exact_plan() {
    let world = MutationWorld::new();

    delete(&world).await.unwrap();

    let plan = world.mutations.deletion();
    assert_eq!(plan.tenant_id, USER_TENANT);
    assert_eq!(plan.chat_id, CHAT);
    assert_eq!(plan.user_id, USER);
    assert_eq!(plan.target_turn_id, TURN);
    assert_eq!(plan.target_request_id, REQUEST);
    assert_eq!(plan.target_started_at, at(-60));
    assert_eq!(
        plan.target_user_message,
        MessageBoundary {
            created_at: at(-61),
            id: TARGET_USER_MESSAGE
        }
    );
    assert_eq!(
        plan.now,
        crate::domain::service::quota::test_support::noon()
    );
    assert_eq!(
        plan.audit,
        TurnMutationAuditEvent::TurnDelete {
            tenant_id: USER_TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            request_id: REQUEST,
            timestamp: plan.now,
        }
    );
}

#[tokio::test]
async fn a_delete_is_authorized_once_as_delete_turn_on_the_chat() {
    let world = MutationWorld::new();

    delete(&world).await.unwrap();

    let requests = world.pdp.requests();
    assert_eq!(requests.len(), 1, "one evaluation");
    assert_eq!(requests[0].action.name, "delete_turn");
    assert_eq!(requests[0].resource.id, Some(CHAT));
}

#[tokio::test]
async fn a_delete_runs_no_quota_step_no_model_check_and_no_stream() {
    let world = MutationWorld::new();

    delete(&world).await.unwrap();

    assert!(
        world.quota_repo.calls().is_empty(),
        "no preflight, no reserve"
    );
    assert!(
        world.policy.calls.lock().unwrap().is_empty(),
        "no catalog check"
    );
    assert_eq!(world.provider.open_count(), 0);
    assert!(
        world.turns.calls().is_empty(),
        "the turn store of the pipeline is not used"
    );
}

#[tokio::test]
async fn a_delete_records_ok_and_one_latency_sample() {
    let world = MutationWorld::new();

    delete(&world).await.unwrap();

    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Delete, MutationResult::Ok)]
    );
    let latencies = world.metrics.latencies();
    assert_eq!(latencies.len(), 1);
    assert_eq!(latencies[0].0, MutationOp::Delete);
    assert!(latencies[0].1 >= 0.0);
}

#[tokio::test]
async fn a_delete_refused_by_the_preview_records_its_reason_and_no_latency() {
    let cases: Vec<(&str, Box<dyn Fn(&MutationWorld)>, MutationResult)> = vec![
        (
            "a newer turn",
            Box::new(|world| {
                let newer = other_turn(1, TurnState::Completed, 10);
                world.mutations.with(|inner| {
                    inner.latest = Some(LatestTurn {
                        id: newer.id,
                        started_at: newer.started_at,
                    });
                });
            }),
            MutationResult::NotLatestTurn,
        ),
        (
            "a soft-deleted target",
            Box::new(|world| {
                world
                    .mutations
                    .with(|inner| inner.targets[0].turn.deleted = true)
            }),
            MutationResult::NotLatestTurn,
        ),
        (
            "a running target",
            Box::new(|world| {
                world
                    .mutations
                    .with(|inner| inner.targets[0].turn.state = TurnState::Running);
            }),
            MutationResult::NotTerminal,
        ),
        (
            "an unknown turn",
            Box::new(|world| world.mutations.with(|inner| inner.targets.clear())),
            MutationResult::Rejected,
        ),
        (
            "a foreign requester",
            Box::new(|world| {
                world.mutations.with(|inner| {
                    inner.targets[0].turn.requester_user_id = Some(OTHER_USER);
                });
            }),
            MutationResult::Rejected,
        ),
    ];
    for (name, arrange, expected) in cases {
        let world = MutationWorld::new();
        arrange(&world);

        let error = refusal(delete(&world).await);

        assert_eq!(
            MutationResult::before_commit(&error),
            expected,
            "{name}: {error:?}"
        );
        assert_eq!(
            result_of(&world),
            vec![(MutationOp::Delete, expected)],
            "{name}"
        );
        assert!(world.metrics.latencies().is_empty(), "{name}");
        assert_eq!(
            world
                .mutations
                .count(|call| matches!(call, MutationCall::Delete(_))),
            0,
            "{name}: the transaction never opened"
        );
    }
}

#[tokio::test]
async fn a_delete_that_loses_the_transaction_race_is_not_the_latest_turn() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .delete_results
            .push_back(Err(DomainError::NotLatestTurn))
    });

    let error = refusal(delete(&world).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Delete, MutationResult::NotLatestTurn)]
    );
    assert!(
        world.metrics.latencies().is_empty(),
        "committed mutations only"
    );
}

#[tokio::test]
async fn a_failed_delete_transaction_is_an_error_result() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .delete_results
            .push_back(Err(DomainError::Database("down".to_owned())));
    });

    let error = refusal(delete(&world).await);

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Delete, MutationResult::Error)]
    );
}

#[tokio::test]
async fn a_delete_is_never_retried_by_the_service() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .delete_results
            .push_back(Err(DomainError::Database("busy".to_owned())));
    });

    let _ = delete(&world).await;

    assert_eq!(
        world
            .mutations
            .count(|call| matches!(call, MutationCall::Delete(_))),
        1,
        "the bounded conflict retry lives in the store transaction"
    );
}

#[tokio::test]
async fn a_pdp_deny_on_delete_is_authz_denied_and_nothing_is_read() {
    let world = MutationWorld::build(Some(chat("p")), PdpReply::Deny);

    let error = refusal(delete(&world).await);

    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
    assert!(world.chats.loads.lock().unwrap().is_empty());
    assert!(world.mutations.calls().is_empty());
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Delete, MutationResult::Rejected)]
    );
}

#[tokio::test]
async fn a_pdp_failure_on_delete_is_pdp_unavailable() {
    let world = MutationWorld::build(Some(chat("p")), PdpReply::Failure);

    let error = refusal(delete(&world).await);

    assert!(matches!(error, DomainError::PdpUnavailable), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Delete, MutationResult::Rejected)]
    );
}

#[tokio::test]
async fn an_unknown_chat_on_delete_is_a_missing_chat_and_no_turn_is_read() {
    let world = MutationWorld::build(None, PdpReply::AllowScoped);

    let error = refusal(delete(&world).await);

    assert!(matches!(
        error,
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    ));
    assert!(world.mutations.calls().is_empty());
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Delete, MutationResult::Rejected)]
    );
}

// ==== slice 1 and 2: the check order of retry and edit ===============================================

#[tokio::test]
async fn an_empty_edit_is_refused_before_any_pdp_call_or_read() {
    for content in ["", "   ", "\n\t "] {
        let world = MutationWorld::new();

        let error = refusal(edit(&world, content).await);

        assert!(
            matches!(error, DomainError::EmptyContent),
            "{content:?}: {error:?}"
        );
        assert!(world.pdp.requests().is_empty(), "{content:?}");
        assert!(world.chats.loads.lock().unwrap().is_empty());
        assert!(world.mutations.calls().is_empty());
        assert!(world.turns.calls().is_empty());
        assert_eq!(
            result_of(&world),
            vec![(MutationOp::Edit, MutationResult::Rejected)]
        );
    }
}

#[tokio::test]
async fn empty_content_wins_on_a_chat_the_caller_cannot_see() {
    let world = MutationWorld::build(None, PdpReply::Deny);

    let error = refusal(edit(&world, "  ").await);

    assert!(matches!(error, DomainError::EmptyContent), "{error:?}");
}

#[tokio::test]
async fn retry_and_edit_are_authorized_once_each_with_their_own_action() {
    for (op, expected) in [
        (MutationOp::Retry, "retry_turn"),
        (MutationOp::Edit, "edit_turn"),
    ] {
        let world = MutationWorld::new();
        world.provider.expect_stream();

        let _ = opened(match op {
            MutationOp::Retry => retry(&world).await,
            _ => edit(&world, "new text").await,
        });

        let requests = world.pdp.requests();
        assert_eq!(
            requests.len(),
            1,
            "{op:?}: the preflight and the stream add no PDP call"
        );
        assert_eq!(requests[0].action.name, expected);
        assert_eq!(requests[0].resource.id, Some(CHAT));
    }
}

#[tokio::test]
async fn a_pdp_deny_on_retry_and_edit_changes_nothing() {
    for edit_op in [false, true] {
        let world = MutationWorld::build(Some(chat("p")), PdpReply::Deny);

        let error = refusal(if edit_op {
            edit(&world, "x").await
        } else {
            retry(&world).await
        });

        assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
        assert!(world.mutations.calls().is_empty());
        assert_untouched(&world);
    }
}

#[tokio::test]
async fn a_pdp_failure_on_retry_is_pdp_unavailable() {
    let world = MutationWorld::build(Some(chat("p")), PdpReply::Failure);

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::PdpUnavailable), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::Rejected)]
    );
    assert_untouched(&world);
}

#[tokio::test]
async fn the_pre_commit_reads_come_before_the_transaction_in_the_order_of_the_feature() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let calls = world.mutations.calls();
    let position = |wanted: &dyn Fn(&MutationCall) -> bool| calls.iter().position(wanted);
    let replace = position(&|c| matches!(c, MutationCall::Replace(_))).unwrap();
    let find = position(&|c| matches!(c, MutationCall::FindTarget(..))).unwrap();
    let latest = position(&|c| matches!(c, MutationCall::Latest(_))).unwrap();
    let message = position(&|c| matches!(c, MutationCall::UserMessage(..))).unwrap();
    let carried = position(&|c| matches!(c, MutationCall::Carried(..))).unwrap();
    let boundary = position(&|c| matches!(c, MutationCall::Boundary(..))).unwrap();
    assert!(
        find < latest && latest < message && message < carried,
        "{calls:?}"
    );
    assert!(carried < replace, "{calls:?}");
    assert!(
        replace < boundary,
        "the history is read after the commit: {calls:?}"
    );
}

// ==== slice 2: refusals before the commit leave the previous turn alone ===========================

fn long_content_catalog() -> Vec<mini_chat_sdk::ModelCatalogEntry> {
    let mut tight = model("p", ModelTier::Premium)
        .text_budgets(4, 100, 10)
        .build();
    tight.max_input_tokens = 1000;
    vec![tight, model("s", ModelTier::Standard).build()]
}

#[tokio::test]
async fn every_refusal_before_the_commit_leaves_the_previous_turn_alone_and_records_one_result() {
    type Arrange = Box<dyn Fn(&MutationWorld)>;
    let cases: Vec<(&str, Arrange, MutationResult)> = vec![
        (
            "the chat model left the catalog",
            Box::new(|world| {
                world.chats.chat.lock().unwrap().as_mut().unwrap().model = "gone".to_owned();
            }),
            MutationResult::Rejected,
        ),
        (
            "a policy plugin failure",
            Box::new(|world| world.policy.fail(PolicyStep::Snapshot)),
            MutationResult::Error,
        ),
        (
            "the policy version cannot be read",
            Box::new(|world| world.policy.fail(PolicyStep::Version)),
            MutationResult::Error,
        ),
        (
            "too many carried images",
            Box::new(|world| {
                let images: Vec<_> = (1..=5).map(image).collect();
                world
                    .mutations
                    .with(|inner| inner.carried = images.iter().map(|row| row.id).collect());
                world.turns.with(|inner| inner.request_attachments = images);
            }),
            MutationResult::Rejected,
        ),
        (
            "a failed carried-link read",
            Box::new(|world| {
                world
                    .mutations
                    .with(|inner| inner.read_error = Some("down".to_owned()));
            }),
            MutationResult::Error,
        ),
    ];
    for (name, arrange, expected) in cases {
        let world = MutationWorld::new();
        arrange(&world);

        let error = refusal(retry(&world).await);

        assert_eq!(
            MutationResult::before_commit(&error),
            expected,
            "{name}: {error:?}"
        );
        assert_eq!(
            result_of(&world),
            vec![(MutationOp::Retry, expected)],
            "{name}"
        );
        assert!(world.metrics.latencies().is_empty(), "{name}");
        assert_untouched(&world);
    }
}

#[tokio::test]
async fn the_chat_model_check_is_invalid_model() {
    let world = MutationWorld::new();
    world.chats.chat.lock().unwrap().as_mut().unwrap().model = "gone".to_owned();

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::InvalidModel), "{error:?}");
}

#[tokio::test]
async fn too_many_images_is_refused_before_the_preflight() {
    let world = MutationWorld::new();
    let images: Vec<_> = (1..=5).map(image).collect();
    world
        .mutations
        .with(|inner| inner.carried = images.iter().map(|row| row.id).collect());
    world.turns.with(|inner| inner.request_attachments = images);

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::TooManyImages), "{error:?}");
    assert!(world.quota_repo.calls().is_empty(), "no quota read");
}

#[tokio::test]
async fn the_preflight_uses_the_web_search_flag_of_the_replaced_turn() {
    // With the kill switch on, a turn sent with web search cannot be retried;
    // a turn sent without it can, even though the model supports web search.
    let sent_with = world_with_switches(switches(|s| s.disable_web_search = true));
    sent_with
        .mutations
        .with(|inner| inner.targets[0].web_search_enabled = true);
    let sent_without = world_with_switches(switches(|s| s.disable_web_search = true));
    sent_without.provider.expect_stream();

    let refused = refusal(retry(&sent_with).await);
    let accepted = opened(retry(&sent_without).await);

    assert!(
        matches!(
            refused,
            DomainError::FeatureDisabled(DisabledFeature::WebSearch)
        ),
        "{refused:?}"
    );
    assert_untouched(&sent_with);
    drop(accepted);
}

#[tokio::test]
async fn an_exhausted_quota_is_refused_before_the_commit() {
    use crate::domain::service::quota::ports::PreflightRows;
    use crate::domain::service::quota::test_support::design_rows;
    let world = MutationWorld::new();
    *world.quota_repo.rows.lock().unwrap() = PreflightRows {
        prior_context_tokens: 0,
        buckets: design_rows()
            .into_iter()
            .map(|mut row| {
                row.spent_credits_micro = 1_000_000_000_000;
                row
            })
            .collect(),
    };

    let error = refusal(retry(&world).await);

    assert!(
        matches!(error, DomainError::QuotaExceeded(QuotaSubject::Tokens)),
        "{error:?}"
    );
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::Rejected)]
    );
    assert_untouched(&world);
}

#[tokio::test]
async fn input_too_long_checks_the_original_content_on_retry() {
    let world = MutationWorld::new().with_catalog(long_content_catalog());
    world
        .mutations
        .with(|inner| inner.user_message.as_mut().unwrap().content = "x".repeat(4000));

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::InputTooLong), "{error:?}");
    assert_untouched(&world);
}

#[tokio::test]
async fn input_too_long_checks_the_edited_content_not_the_original_on_edit() {
    let world = MutationWorld::new().with_catalog(long_content_catalog());
    world
        .mutations
        .with(|inner| inner.user_message.as_mut().unwrap().content = "x".repeat(4000));
    world.provider.expect_stream();

    let long = refusal(edit(&world, &"y".repeat(4000)).await);
    let short = opened(edit(&world, "short").await);

    assert!(matches!(long, DomainError::InputTooLong), "{long:?}");
    drop(short);
}

#[tokio::test]
async fn carried_images_are_refused_under_their_kill_switch_never_dropped() {
    let world = world_with_switches(switches(|s| s.disable_images = true));
    let picture = image(1);
    world
        .mutations
        .with(|inner| inner.carried = vec![picture.id]);
    world
        .turns
        .with(|inner| inner.request_attachments = vec![picture]);

    let error = refusal(retry(&world).await);

    assert!(
        matches!(error, DomainError::FeatureDisabled(DisabledFeature::Images)),
        "{error:?}"
    );
    assert_untouched(&world);
}

#[tokio::test]
async fn carried_images_on_a_model_without_vision_are_refused() {
    let world = MutationWorld::new().with_catalog(vec![
        model("p", ModelTier::Premium).vision(false).build(),
        model("s", ModelTier::Standard).vision(false).build(),
    ]);
    let picture = image(1);
    world
        .mutations
        .with(|inner| inner.carried = vec![picture.id]);
    world
        .turns
        .with(|inner| inner.request_attachments = vec![picture]);

    let error = refusal(retry(&world).await);

    assert!(
        matches!(error, DomainError::VisionNotSupported),
        "{error:?}"
    );
    assert_untouched(&world);
}

// ==== slice 2: the replacement plan ===================================================================

#[tokio::test]
async fn a_retry_replaces_with_the_original_content_and_the_ids_of_a_new_turn() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let (started, _relay) = opened(retry(&world).await);

    let plan = world.mutations.replacement();
    assert_eq!(plan.op, MutationOp::Retry);
    assert_eq!(plan.content, ORIGINAL);
    assert_eq!(plan.tenant_id, USER_TENANT);
    assert_eq!(plan.chat_id, CHAT);
    assert_eq!(plan.user_id, USER);
    assert_eq!(plan.target_turn_id, TURN);
    assert_eq!(plan.target_request_id, REQUEST);
    assert_eq!(plan.target_started_at, at(-60));
    assert_eq!(
        plan.target_user_message,
        MessageBoundary {
            created_at: at(-61),
            id: TARGET_USER_MESSAGE
        }
    );
    assert_eq!(started.request_id, plan.new_request_id);
    assert_eq!(started.message_id, plan.assistant_message_id);
    assert!(started.is_new_turn);
}

#[tokio::test]
async fn the_new_request_id_is_a_server_generated_uuid_v4_and_all_new_ids_differ() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let plan = world.mutations.replacement();
    assert_eq!(plan.new_request_id.get_version_num(), 4);
    let ids = [
        plan.new_turn_id,
        plan.new_request_id,
        plan.new_user_message_id,
        plan.assistant_message_id,
        plan.target_turn_id,
        plan.target_request_id,
    ];
    for (i, a) in ids.iter().enumerate() {
        assert!(!a.is_nil());
        for b in &ids[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[tokio::test]
async fn two_retries_get_two_different_new_request_ids() {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let world = MutationWorld::new();
        world.provider.expect_stream();
        let _ = opened(retry(&world).await);
        seen.push(world.mutations.replacement().new_request_id);
    }

    assert_ne!(seen[0], seen[1]);
}

#[tokio::test]
async fn an_edit_replaces_with_the_content_exactly_as_sent() {
    let world = MutationWorld::new();
    world.provider.expect_stream();
    let sent = "  edited text\n with a tail  \n";

    let _ = opened(edit(&world, sent).await);

    let plan = world.mutations.replacement();
    assert_eq!(plan.op, MutationOp::Edit);
    assert_eq!(plan.content, sent, "not trimmed");
}

#[tokio::test]
async fn the_web_search_flag_of_the_replaced_turn_is_carried_to_the_plan() {
    for flag in [false, true] {
        let world = MutationWorld::new();
        world
            .mutations
            .with(|inner| inner.targets[0].web_search_enabled = flag);
        world.provider.expect_stream();

        let _ = opened(retry(&world).await);

        assert_eq!(world.mutations.replacement().web_search_enabled, flag);
    }
}

#[tokio::test]
async fn the_audit_event_of_a_retry_is_built_before_the_transaction() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let plan = world.mutations.replacement();
    assert_eq!(
        plan.audit,
        TurnMutationAuditEvent::TurnRetry {
            tenant_id: USER_TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            original_request_id: REQUEST,
            new_request_id: plan.new_request_id,
            timestamp: plan.now,
        }
    );
}

#[tokio::test]
async fn the_audit_event_of_an_edit_is_a_turn_edit() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(edit(&world, "new").await);

    let plan = world.mutations.replacement();
    assert_eq!(
        plan.audit,
        TurnMutationAuditEvent::TurnEdit {
            tenant_id: USER_TENANT,
            actor_user_id: USER,
            chat_id: CHAT,
            original_request_id: REQUEST,
            new_request_id: plan.new_request_id,
            timestamp: plan.now,
        }
    );
}

#[tokio::test]
async fn the_mutation_time_is_the_clock_when_it_is_past_the_replaced_rows() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    assert_eq!(
        world.mutations.replacement().now,
        crate::domain::service::quota::test_support::noon()
    );
}

#[tokio::test]
async fn the_mutation_time_is_raised_after_the_replaced_rows_under_clock_skew() {
    let world = MutationWorld::new();
    // The turn started in the future of this pod's clock.
    let mut skewed = target(TurnState::Completed);
    skewed.turn.started_at = at(30);
    world.set_target(skewed);
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let plan = world.mutations.replacement();
    assert_eq!(plan.now, at(30) + time::Duration::microseconds(1));
    assert_eq!(plan.audit_timestamp(), plan.now);
}

trait AuditTimestamp {
    fn audit_timestamp(&self) -> time::OffsetDateTime;
}

impl AuditTimestamp for crate::domain::service::mutations::ports::ReplacementPlan {
    fn audit_timestamp(&self) -> time::OffsetDateTime {
        match &self.audit {
            TurnMutationAuditEvent::TurnRetry { timestamp, .. }
            | TurnMutationAuditEvent::TurnEdit { timestamp, .. }
            | TurnMutationAuditEvent::TurnDelete { timestamp, .. } => *timestamp,
        }
    }
}

// ==== slice 2: conflicts ==============================================================================

#[tokio::test]
async fn a_unique_violation_with_a_running_turn_is_generation_in_progress() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::UniqueViolation))
    });
    world
        .turns
        .with(|inner| inner.running = Some(other_running()));

    let error = refusal(retry(&world).await);

    assert!(
        matches!(error, DomainError::GenerationInProgress),
        "{error:?}"
    );
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::GenerationInProgress)]
    );
    assert_eq!(
        world
            .turns
            .count(|call| matches!(call, TurnsCall::FindRunning(chat) if *chat == CHAT)),
        1,
        "the running turn is re-queried because the violation names no constraint"
    );
}

#[tokio::test]
async fn no_running_turn_after_unique_violation_is_not_latest_turn() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::UniqueViolation))
    });
    // No running turn exists: the winner already left running.

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::NotLatestTurn)]
    );
    assert_nothing_after_the_transaction(&world);
}

#[tokio::test]
async fn running_turn_finalized_between_violation_and_requery_returns_not_latest_turn() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::UniqueViolation))
    });

    // Simulate the race condition where the winner's turn exists initially,
    // but is finalized (leaves running state) before this loser re-queries.
    world
        .turns
        .with(|inner| inner.running = Some(other_running()));

    // Clear the running turn to simulate it being finalized between the
    // unique violation and the re-query of find_running.
    world.turns.with(|inner| inner.running = None);

    let error = refusal(retry(&world).await);

    // The result is NotLatestTurn because the running turn is gone.
    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::NotLatestTurn)]
    );
    // Verify that find_running was called to detect the finalized turn.
    assert_eq!(
        world
            .turns
            .count(|call| matches!(call, TurnsCall::FindRunning(chat) if *chat == CHAT)),
        1,
        "find_running was called to re-query after the unique violation"
    );
    assert_nothing_after_the_transaction(&world);
}

#[tokio::test]
async fn a_unique_violation_is_not_retried() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::UniqueViolation))
    });
    world
        .turns
        .with(|inner| inner.running = Some(other_running()));

    let _ = retry(&world).await;

    assert_eq!(
        world
            .mutations
            .count(|call| matches!(call, MutationCall::Replace(_))),
        1
    );
}

#[tokio::test]
async fn a_replacement_that_finds_the_target_gone_is_not_the_latest_turn() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::NotLatestTurn))
    });

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::NotLatestTurn)]
    );
    assert!(
        world.metrics.latencies().is_empty(),
        "nothing was committed"
    );
    assert_nothing_after_the_transaction(&world);
}

#[tokio::test]
async fn a_failed_replacement_transaction_is_a_server_error_and_marks_nothing() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::Database("down".to_owned())));
    });

    let error = refusal(retry(&world).await);

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::Error)]
    );
    assert!(
        world.mutations.marks().is_empty(),
        "no new turn exists to mark"
    );
    assert!(world.metrics.latencies().is_empty());
}

// ==== slice 3: the post-commit setup =================================================================

#[tokio::test]
async fn the_history_excludes_the_new_request_and_feeds_the_provider_request() {
    let world = MutationWorld::new();
    let kept = vec![
        history(1, HistoryRole::User, "earlier question", 1),
        history(2, HistoryRole::Assistant, "earlier answer", 2),
    ];
    world.mutations.with(|inner| {
        inner.boundary = Some(MessageBoundary {
            created_at: at(-200),
            id: Uuid::from_u128(0x99),
        });
    });
    world.turns.with(|inner| inner.recent.clone_from(&kept));
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let plan = world.mutations.replacement();
    assert!(
        world
            .mutations
            .calls()
            .contains(&MutationCall::Boundary(CHAT, plan.new_request_id))
    );
    let builds = world.provider.builds();
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].plan.history, kept);
    assert_eq!(
        builds[0].plan.current.as_ref().unwrap().content,
        ORIGINAL,
        "the current message is the original content"
    );
}

#[tokio::test]
async fn an_edit_sends_the_edited_content_as_the_current_message() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(edit(&world, "edited question").await);

    assert_eq!(
        world.provider.builds()[0]
            .plan
            .current
            .as_ref()
            .unwrap()
            .content,
        "edited question"
    );
}

#[tokio::test]
async fn the_kept_summary_is_applied_and_reported_in_stream_started() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| {
        inner.boundary = Some(MessageBoundary {
            created_at: at(-200),
            id: Uuid::from_u128(0x99),
        });
    });
    world.turns.with(|inner| {
        inner.summary = Some(ThreadSummaryInfo {
            summary_text: "the summary".to_owned(),
            frontier: MessageBoundary {
                created_at: at(-300),
                id: Uuid::from_u128(0x98),
            },
            token_estimate: 42,
        });
    });
    world.provider.expect_stream();

    let (started, _relay) = opened(retry(&world).await);

    assert_eq!(started.thread_summary_applied, Some(42));
    assert!(world.provider.builds()[0].plan.summary.is_some());
}

#[tokio::test]
async fn the_carried_images_are_resent_with_the_replacement() {
    let world = MutationWorld::new();
    let picture = image(1);
    world
        .mutations
        .with(|inner| inner.carried = vec![picture.id]);
    world
        .turns
        .with(|inner| inner.request_attachments = vec![picture.clone()]);
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let images = world.provider.builds()[0]
        .plan
        .current
        .as_ref()
        .unwrap()
        .images
        .clone();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].id, picture.id);
}

#[tokio::test]
async fn the_reserve_runs_in_fill_mode_for_the_new_turn_as_the_last_setup_step() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    let plan = world.mutations.replacement();
    let reserves = world.quota_store.reserves();
    assert_eq!(reserves.len(), 1);
    assert_eq!(
        reserves[0].mode,
        ReserveMode::Fill {
            turn_id: plan.new_turn_id
        }
    );
    assert_eq!(reserves[0].tenant_id, USER_TENANT);
    assert_eq!(reserves[0].user_id, USER);
    assert_eq!(
        world.provider.builds().len(),
        1,
        "the request was built before the reserve"
    );
    assert_eq!(
        world.turns.creates().len(),
        0,
        "the 0004 create-turn statements are not run"
    );
}

async fn assert_setup_failure(
    world: &MutationWorld,
    result: Result<SendOutcome, DomainError>,
    code: &str,
) -> DomainError {
    let error = refusal(result);
    let plan = world.mutations.replacement();
    assert_eq!(
        world.mutations.marks(),
        vec![(plan.new_turn_id, code.to_owned())],
        "the new turn is marked failed with `{code}`"
    );
    assert_eq!(
        world.mutations.with(|inner| {
            inner
                .calls
                .iter()
                .filter_map(|call| match call {
                    MutationCall::Mark { tenant, .. } => Some(*tenant),
                    _ => None,
                })
                .collect::<Vec<_>>()
        }),
        vec![USER_TENANT]
    );
    assert_eq!(
        result_of(world),
        vec![(world_op(&plan), MutationResult::SetupFailed)]
    );
    assert_eq!(world.provider.open_count(), 0, "no stream was opened");
    assert!(
        world.turns.finalizations().is_empty(),
        "no settlement, no audit event"
    );
    assert_eq!(
        world.metrics.latencies().len(),
        1,
        "the mutation itself committed"
    );
    error
}

fn world_op(plan: &crate::domain::service::mutations::ports::ReplacementPlan) -> MutationOp {
    plan.op
}

#[tokio::test]
async fn a_context_budget_failure_marks_context_length_exceeded_and_returns_it() {
    let mut tiny = model("p", ModelTier::Premium).build();
    tiny.context_window = 400; // below the 500 output tokens that apply
    let world =
        MutationWorld::new().with_catalog(vec![tiny, model("s", ModelTier::Standard).build()]);

    let error = assert_setup_failure(&world, retry(&world).await, "context_length_exceeded").await;

    assert!(
        matches!(error, DomainError::ContextBudgetExceeded),
        "{error:?}"
    );
    assert!(
        world.quota_store.reserves().is_empty(),
        "no reserve was attempted"
    );
}

#[tokio::test]
async fn a_failed_history_read_marks_turn_setup_failed_and_returns_the_database_error() {
    let world = MutationWorld::new();
    world
        .mutations
        .with(|inner| inner.history_error = Some("history down".to_owned()));

    let error = assert_setup_failure(&world, retry(&world).await, "turn_setup_failed").await;

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
}

#[tokio::test]
async fn a_provider_resolution_failure_marks_turn_setup_failed() {
    let world = MutationWorld::new();
    world
        .directory
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let error = assert_setup_failure(&world, retry(&world).await, "turn_setup_failed").await;

    assert!(
        matches!(error, DomainError::ProviderResolution(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_request_building_failure_marks_turn_setup_failed_before_any_reserve() {
    let world = MutationWorld::new();
    world.provider.fail_build("cannot serialize");

    let error = assert_setup_failure(&world, edit(&world, "x").await, "turn_setup_failed").await;

    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    assert!(world.quota_store.reserves().is_empty());
}

#[tokio::test]
async fn a_failed_reserve_recheck_marks_quota_exceeded_and_is_a_tokens_429() {
    let world = MutationWorld::new();
    world
        .quota_store
        .script_reserve(Err(DomainError::QuotaExceeded(QuotaSubject::Tokens)));

    let error = assert_setup_failure(&world, retry(&world).await, "quota_exceeded").await;

    assert!(
        matches!(error, DomainError::QuotaExceeded(QuotaSubject::Tokens)),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_reserve_database_error_or_a_fill_that_updates_no_row_marks_turn_setup_failed() {
    for failure in [
        DomainError::Database("busy".to_owned()),
        DomainError::Internal(
            "the turn to fill is missing, not running or already reserved".to_owned(),
        ),
    ] {
        let world = MutationWorld::new();
        world.quota_store.script_reserve(Err(failure));

        let error = assert_setup_failure(&world, retry(&world).await, "turn_setup_failed").await;

        assert!(
            matches!(error, DomainError::Database(_) | DomainError::Internal(_)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn a_setup_failure_keeps_the_committed_replacement_and_enqueues_no_further_audit() {
    let world = MutationWorld::new();
    world.provider.fail_build("boom");

    let _ = retry(&world).await;

    assert_eq!(
        world
            .mutations
            .count(|call| matches!(call, MutationCall::Replace(_))),
        1,
        "the replacement is not undone"
    );
    assert_eq!(
        world
            .mutations
            .count(|call| matches!(call, MutationCall::Delete(_))),
        0
    );
    assert!(
        world.turns.finalizations().is_empty(),
        "no turn_failed audit event for a replacement that failed its setup"
    );
}

#[tokio::test(start_paused = true)]
async fn a_mark_that_keeps_failing_still_returns_the_original_error_and_logs_it() {
    let world = MutationWorld::new();
    world.provider.fail_build("boom");
    world.mutations.with(|inner| {
        for _ in 0..5 {
            inner
                .mark_results
                .push_back(Err(DomainError::Database("mark down".to_owned())));
        }
    });
    let logs = LogCapture::start();

    let error = refusal(retry(&world).await);

    assert!(
        matches!(error, DomainError::Internal(_)),
        "the original failure: {error:?}"
    );
    assert_eq!(world.mutations.marks().len(), 3, "bounded attempts");
    assert!(!logs.at(Level::ERROR).is_empty());
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::SetupFailed)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_panic_after_the_commit_marks_the_new_turn_failed_through_the_drop_guard() {
    let world = MutationWorld::new();
    world.mutations.with(|inner| inner.history_panics = true);

    let error = refusal(retry(&world).await);
    settle().await;

    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    let plan = world.mutations.replacement();
    assert_eq!(
        world.mutations.marks(),
        vec![(plan.new_turn_id, "turn_setup_failed".to_owned())]
    );
    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Retry, MutationResult::SetupFailed)]
    );
}

// ==== slice 4: the hand-off and the metrics ===========================================================

#[tokio::test(start_paused = true)]
async fn the_replacement_streams_and_is_finalized_as_a_new_turn() {
    let world = MutationWorld::new();
    let feed = world.provider.expect_stream();

    let (started, relay) = opened(retry(&world).await);
    let mut stream = relay.into_stream(started.clone());
    feed.push_all([text("Hel"), text("lo"), completed(11, 7)]);
    let mut names = Vec::new();
    while let Some(event) = stream.next().await {
        names.push(event.name());
    }

    assert_eq!(names, ["stream_started", "delta", "delta", "done"]);
    let plan = world.mutations.replacement();
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.turn_id, plan.new_turn_id);
    assert_eq!(request.columns.state, TerminalState::Completed);
    assert_eq!(
        request.columns.assistant_message_id,
        Some(started.message_id)
    );
    let message = writes.assistant_message.unwrap();
    assert_eq!(message.id, plan.assistant_message_id);
    assert_eq!(message.request_id, plan.new_request_id);
    assert_eq!(message.content, "Hello");
    assert!(
        message.created_at > plan.now,
        "the answer sorts after the new user message"
    );
    assert_eq!(writes.audit.event_type, TurnAuditEventType::TurnCompleted);
    assert_eq!(writes.audit.request_id, plan.new_request_id);
    assert_eq!(writes.audit.turn_id, plan.new_turn_id);
    assert_eq!(world.provider.open_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_provider_task_runs_with_the_new_turn_and_the_selected_model_of_the_chat() {
    let world = MutationWorld::new();
    let feed = world.provider.expect_stream();

    let (started, relay) = opened(edit(&world, "edited").await);
    let mut stream = relay.into_stream(started);
    feed.push(completed(1, 1));
    while stream.next().await.is_some() {}

    let (request, _) = world.turns.finalizations().remove(0);
    let crate::domain::service::quota::finalize::FinalizeVariant::Stream {
        tenant_id,
        selected_model,
        ..
    } = request.variant
    else {
        panic!("a stream finalization expected");
    };
    assert_eq!(tenant_id, USER_TENANT);
    assert_eq!(selected_model, "p");
}

#[tokio::test]
async fn the_reserve_and_the_image_metrics_of_the_send_flow_are_recorded() {
    let world = MutationWorld::new();
    let picture = image(1);
    world
        .mutations
        .with(|inner| inner.carried = vec![picture.id]);
    world
        .turns
        .with(|inner| inner.request_attachments = vec![picture]);
    world.provider.expect_stream();

    let _ = opened(retry(&world).await);

    assert_eq!(
        world
            .quota_metrics
            .events()
            .iter()
            .filter(|metric| matches!(
                metric,
                crate::domain::service::quota::test_support::Metric::Reserve(_)
            ))
            .count(),
        2,
        "one per period"
    );
    assert_eq!(
        world.stream_metrics.count(|metric| *metric
            == crate::domain::service::stream::test_support::StreamMetric::ImageInputs(1)),
        1
    );
}

#[tokio::test]
async fn a_handed_over_replacement_records_ok_and_one_latency_sample() {
    let world = MutationWorld::new();
    world.provider.expect_stream();

    let _ = opened(edit(&world, "new").await);

    assert_eq!(
        result_of(&world),
        vec![(MutationOp::Edit, MutationResult::Ok)]
    );
    let latencies = world.metrics.latencies();
    assert_eq!(latencies.len(), 1);
    assert_eq!(latencies[0].0, MutationOp::Edit);
}

#[tokio::test]
async fn every_return_path_records_exactly_one_result() {
    // Success, refusal, conflict, setup failure: one record each, never two.
    let ok = MutationWorld::new();
    ok.provider.expect_stream();
    let _ = opened(retry(&ok).await);
    let conflict = MutationWorld::new();
    conflict.mutations.with(|inner| {
        inner
            .replace_results
            .push_back(Err(DomainError::NotLatestTurn))
    });
    let _ = retry(&conflict).await;
    let setup = MutationWorld::new();
    setup.provider.fail_build("x");
    let _ = retry(&setup).await;
    let denied = MutationWorld::build(Some(chat("p")), PdpReply::Deny);
    let _ = retry(&denied).await;

    for world in [&ok, &conflict, &setup, &denied] {
        assert_eq!(
            world.metrics.results().len(),
            1,
            "{:?}",
            world.metrics.results()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_right_after_the_commit_still_ends_the_new_turn_cancelled() {
    let world = MutationWorld::new();
    let _feed = world.provider.expect_stream();
    let committed = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let (committed, proceed) = (Arc::clone(&committed), Arc::clone(&proceed));
        *world.mutations.after_replace.lock().unwrap() = Some(Arc::new(move || {
            let (committed, proceed) = (Arc::clone(&committed), Arc::clone(&proceed));
            Box::pin(async move {
                committed.notify_one();
                proceed.notified().await;
            })
        }));
    }
    let service = Arc::clone(&world.service);
    let mut future = Box::pin(async move { service.retry(ctx(), CHAT, REQUEST).await.map(|_| ()) });
    tokio::select! {
        _ = &mut future => panic!("the retry cannot complete while the commit hook waits"),
        () = committed.notified() => {}
    }
    drop(future); // the handler is gone
    proceed.notify_one();

    wait_for_finalization(&world).await;
    settle().await;

    let plan = world.mutations.replacement();
    let (request, writes) = world.turns.finalizations().remove(0);
    assert_eq!(request.turn_id, plan.new_turn_id);
    assert_eq!(request.columns.state, TerminalState::Cancelled);
    assert_eq!(writes.assistant_message, None);
    assert_eq!(
        world.provider.open_count(),
        0,
        "the provider never heard of it"
    );
    assert!(
        world.mutations.marks().is_empty(),
        "cancelled, not a setup failure"
    );
    assert_eq!(world.turns.finalizations().len(), 1);
}

#[tokio::test]
async fn the_reserve_is_not_recorded_by_a_refused_mutation() {
    let world = MutationWorld::new();
    world.chats.chat.lock().unwrap().as_mut().unwrap().model = "gone".to_owned();

    let _ = retry(&world).await;

    assert!(world.quota_metrics.events().is_empty());
    assert!(world.stream_metrics.events().is_empty());
    // A healthy retry of the same world does record them.
    let healthy = MutationWorld::new();
    healthy.provider.expect_stream();
    let _ = opened(retry(&healthy).await);
    assert!(!healthy.quota_metrics.events().is_empty());
}

#[tokio::test]
async fn the_same_world_can_retry_the_replacement_again() {
    // A retry of the replacement creates another replacement: the target of the
    // second call is the first call's new turn.
    let world = MutationWorld::new();
    world.provider.expect_stream();
    let _ = opened(retry(&world).await);
    let first = world.mutations.replacement();
    let mut second_target = target(TurnState::Completed);
    second_target.turn.id = first.new_turn_id;
    second_target.turn.request_id = first.new_request_id;
    second_target.turn.started_at = first.now;
    world.mutations.with(|inner| {
        inner.latest = Some(latest_of(&second_target));
        inner.targets = vec![second_target];
        inner.user_message = Some(crate::domain::service::mutations::types::TargetMessage {
            id: first.new_user_message_id,
            content: first.content.clone(),
            created_at: first.now,
        });
    });
    world.provider.expect_stream();

    let _ = opened(world.service.retry(ctx(), CHAT, first.new_request_id).await);

    let plans: Vec<_> = world.mutations.with(|inner| {
        inner
            .calls
            .iter()
            .filter_map(|call| match call {
                MutationCall::Replace(plan) => Some(plan.target_request_id),
                _ => None,
            })
            .collect()
    });
    assert_eq!(plans, vec![REQUEST, first.new_request_id]);
    let _ = (
        target_message(),
        ReservedColumns::from_decision(&premium_decision()),
    );
}
