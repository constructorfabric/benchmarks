//! The error contract, the declared responses, the metrics and the deferred scope of the turn
//! mutation and reaction endpoints: the canonical Problem of each row of DESIGN 3.3 `Error Codes`
//! (`NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `turn_state`, `reaction_target`,
//! `INVALID_REACTION`, an oversized audit payload as a 500), the generic 409 detail, the `OpenAPI`
//! operations of the five endpoints with their declared statuses (415 is not declared), the
//! `mini_chat_turn_mutation{op,result}` and `mini_chat_turn_mutation_latency_ms{op}` series
//! through the real endpoints, and the scope that stays unimplemented (no mutation of older turns,
//! no restore or purge, no client-chosen `request_id`, no reaction on user or system messages).
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract` (`inst-ec-match`,
//! `inst-ec-404`, `inst-ec-body`, `inst-ec-values`, `inst-ec-state`, `inst-ec-409`,
//! `inst-ec-requester`, `inst-ec-preflight`, `inst-ec-setup`, `inst-ec-authz`, `inst-ec-500`,
//! `inst-ec-json-only`, `inst-ec-detail`, `inst-ec-openapi`, `inst-ec-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics` (`inst-met-counter`,
//! `inst-met-latency`, `inst-met-cardinality`, `inst-met-no-reaction-metric`); flow
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability`
//! (`inst-obs-mutation-counter`, `inst-obs-mutation-latency`, `inst-obs-audit-health`,
//! `inst-obs-logs`, `inst-obs-return`); `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`
//! and `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward` (the
//! deferred scope); definitions of done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-errors-observability`
//! and `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-deferred-scope`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::response::IntoResponse;
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use crate::domain::error::{ConflictReason, DomainError, EnqueueSite, ResourceKind};
use crate::test_support::chats::{a1, message_rows, request};
use crate::test_support::mutations::{
    FailingMutationOutbox, assert_internal, seed, sse_of, started_ids, streaming_chat, turn,
};
use crate::test_support::provider::{MockProvider, Reply, happy_steps};
use crate::test_support::quota::{FixedClock, default_now};
use crate::test_support::send::{
    SendRig, SendRigOptions, send_rig, send_rig_with, turn_rows, turn_uri,
};

fn problem(err: DomainError) -> Value {
    serde_json::to_value(Problem::from(CanonicalError::from(err))).expect("problem serializes")
}

// ── the rows of the error table ──────────────────────────────────────────────

#[test]
fn a_mutation_conflict_is_a_409_aborted_with_its_reason_and_a_generic_detail() {
    // inst-ec-409, inst-ec-detail
    for (reason, wire) in [
        (ConflictReason::NotLatestTurn, "NOT_LATEST_TURN"),
        (
            ConflictReason::GenerationInProgress,
            "GENERATION_IN_PROGRESS",
        ),
    ] {
        let value = problem(DomainError::Conflict { reason });
        assert_eq!(value["status"], 409, "{value}");
        assert!(
            value["type"].as_str().unwrap().contains("aborted"),
            "{value}"
        );
        assert_eq!(value["context"]["reason"], wire, "{value}");
        let detail = value["detail"].as_str().unwrap().to_lowercase();
        for leak in [
            "sqlite",
            "unique",
            "constraint",
            "chat_turns",
            "uuid",
            "running",
        ] {
            assert!(!detail.contains(leak), "{leak} in {detail}");
        }
    }
}

#[test]
fn the_two_mutation_conflicts_are_told_apart_by_the_reason_alone() {
    let a = problem(DomainError::Conflict {
        reason: ConflictReason::NotLatestTurn,
    });
    let b = problem(DomainError::Conflict {
        reason: ConflictReason::GenerationInProgress,
    });
    assert_eq!(a["status"], b["status"]);
    assert_ne!(a["context"]["reason"], b["context"]["reason"]);
}

#[test]
fn a_non_terminal_turn_is_a_400_failed_precondition_with_the_turn_state_violation() {
    // inst-ec-state
    let value = problem(DomainError::TurnNotTerminal);
    assert_eq!(value["status"], 400, "{value}");
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("failed_precondition"),
        "{value}"
    );
    let violations = value["context"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{value}");
    assert_eq!(violations[0]["subject"], "turn_state");
    assert_eq!(violations[0]["type"], "STATE");
}

#[test]
fn a_reaction_on_a_non_assistant_message_is_a_400_failed_precondition_reaction_target() {
    // inst-ec-state
    let value = problem(DomainError::ReactionTargetNotAssistant);
    assert_eq!(value["status"], 400, "{value}");
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("failed_precondition"),
        "{value}"
    );
    let violations = value["context"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{value}");
    assert_eq!(violations[0]["subject"], "reaction_target");
    assert_eq!(violations[0]["type"], "STATE");
}

#[test]
fn an_invalid_reaction_is_a_400_invalid_argument_on_the_reaction_field() {
    // inst-ec-values
    let value = problem(DomainError::InvalidReaction);
    assert_eq!(value["status"], 400, "{value}");
    assert!(
        value["type"].as_str().unwrap().contains("invalid_argument"),
        "{value}"
    );
    let violations = value["context"]["field_violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{value}");
    assert_eq!(violations[0]["field"], "reaction");
    assert_eq!(violations[0]["reason"], "INVALID_REACTION");
}

#[test]
fn the_requester_mismatch_is_a_403_permission_denied_with_authz_denied() {
    // inst-ec-requester
    let value = problem(DomainError::AuthzDenied);
    assert_eq!(value["status"], 403);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("permission_denied")
    );
    assert_eq!(value["context"]["reason"], "AUTHZ_DENIED");
}

#[test]
fn an_oversized_mutation_audit_payload_is_a_generic_500_not_the_400_of_chat_deletion() {
    // inst-ec-500, inst-mc-return-payload-500
    for site in [
        EnqueueSite::TurnRetry,
        EnqueueSite::TurnEdit,
        EnqueueSite::TurnDelete,
    ] {
        let value = problem(DomainError::OutboxPayloadTooLarge { site });
        assert_eq!(value["status"], 500, "{site:?}: {value}");
        assert!(
            value["type"].as_str().unwrap().contains("internal"),
            "{value}"
        );
    }
    assert_eq!(
        problem(DomainError::OutboxPayloadTooLarge {
            site: EnqueueSite::ChatDelete
        })["status"],
        400
    );
}

#[test]
fn the_not_found_rows_name_the_turn_and_message_types() {
    // inst-ec-404
    for (kind, resource_type) in [
        (ResourceKind::Turn, "gts.cf.core.mini_chat.turn.v1~"),
        (ResourceKind::Message, "gts.cf.core.mini_chat.message.v1~"),
        (ResourceKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
    ] {
        let value = problem(DomainError::NotFound {
            resource: kind,
            id: None,
        });
        assert_eq!(value["status"], 404);
        assert_eq!(value["context"]["resource_type"], resource_type);
    }
}

#[test]
fn every_new_row_is_a_problem_with_the_contract_fields_and_no_top_level_code() {
    // inst-ec-return
    let errors = [
        DomainError::Conflict {
            reason: ConflictReason::NotLatestTurn,
        },
        DomainError::Conflict {
            reason: ConflictReason::GenerationInProgress,
        },
        DomainError::TurnNotTerminal,
        DomainError::ReactionTargetNotAssistant,
        DomainError::InvalidReaction,
        DomainError::OutboxPayloadTooLarge {
            site: EnqueueSite::TurnEdit,
        },
    ];
    for error in errors {
        let label = format!("{error}");
        let value = problem(error);
        for key in ["type", "title", "status", "detail", "context"] {
            assert!(
                value.get(key).is_some(),
                "{label}: missing `{key}` in {value}"
            );
        }
        assert!(
            value.get("code").is_none(),
            "{label}: top-level code in {value}"
        );
        assert!(
            value["type"].as_str().unwrap().starts_with("gts://"),
            "{label}"
        );
    }
}

#[test]
fn the_new_rows_are_served_as_problem_json_never_as_a_stream() {
    // inst-ec-json-only
    for error in [
        DomainError::TurnNotTerminal,
        DomainError::InvalidReaction,
        DomainError::Conflict {
            reason: ConflictReason::NotLatestTurn,
        },
    ] {
        let response = CanonicalError::from(error).into_response();
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(content_type, "application/problem+json");
    }
}

// ── the declared responses ───────────────────────────────────────────────────

/// `(method, path, operation id, tag, declared statuses, response content type of the 200/204)`.
type Expected = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Vec<u16>,
);

fn expected() -> Vec<Expected> {
    vec![
        (
            "POST",
            "/mini-chat/v1/chats/{id}/turns/{request_id}/retry",
            "mini_chat.retry_turn",
            "Mini Chat Turns",
            vec![200, 400, 401, 403, 404, 409, 429, 500, 503],
        ),
        (
            "PATCH",
            "/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.edit_turn",
            "Mini Chat Turns",
            vec![200, 400, 401, 403, 404, 409, 422, 429, 500, 503],
        ),
        (
            "DELETE",
            "/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.delete_turn",
            "Mini Chat Turns",
            vec![204, 400, 401, 403, 404, 409, 500, 503],
        ),
        (
            "PUT",
            "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
            "mini_chat.put_reaction",
            "Mini Chat Messages",
            vec![200, 400, 401, 403, 404, 422, 500, 503],
        ),
        (
            "DELETE",
            "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
            "mini_chat.delete_reaction",
            "Mini Chat Messages",
            vec![204, 400, 401, 403, 404, 500, 503],
        ),
    ]
}

#[tokio::test]
async fn the_five_operations_are_registered_once_with_their_ids() {
    // inst-ec-openapi
    let rig = send_rig().await;
    for (method, path, operation_id, _, _) in expected() {
        let count = rig
            .openapi
            .ops
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.method.as_str() == method && op.path == path)
            .count();
        assert_eq!(count, 1, "{method} {path}");
        let op = rig.openapi.op(method, path).unwrap();
        assert_eq!(
            op.operation_id.as_deref(),
            Some(operation_id),
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn the_declared_statuses_are_the_documented_ones_and_415_is_not_declared() {
    // inst-ec-openapi
    let rig = send_rig().await;
    for (method, path, _, _, statuses) in expected() {
        let op = rig.openapi.op(method, path).unwrap();
        let declared: BTreeSet<u16> = op.responses.iter().map(|r| r.status).collect();
        assert_eq!(
            declared,
            statuses.iter().copied().collect::<BTreeSet<_>>(),
            "{method} {path}"
        );
        assert!(!declared.contains(&415), "{method} {path}");
        for response in op.responses.iter().filter(|r| r.status >= 400) {
            assert_eq!(
                response.content_type, "application/problem+json",
                "{path} {}",
                response.status
            );
        }
        let unavailable = op.responses.iter().find(|r| r.status == 503).unwrap();
        assert!(
            unavailable
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("retry-after")),
            "{path}: Retry-After on 503"
        );
    }
}

#[tokio::test]
async fn retry_and_edit_declare_an_event_stream_and_delete_and_remove_declare_no_body() {
    let rig = send_rig().await;
    for (method, path) in [
        ("POST", "/mini-chat/v1/chats/{id}/turns/{request_id}/retry"),
        ("PATCH", "/mini-chat/v1/chats/{id}/turns/{request_id}"),
    ] {
        let op = rig.openapi.op(method, path).unwrap();
        let ok = op.responses.iter().find(|r| r.status == 200).unwrap();
        assert_eq!(ok.content_type, "text/event-stream", "{path}");
    }
    let edit = rig
        .openapi
        .op("PATCH", "/mini-chat/v1/chats/{id}/turns/{request_id}")
        .unwrap();
    assert!(edit.request_body.is_some(), "edit declares its JSON body");
    let retry = rig
        .openapi
        .op("POST", "/mini-chat/v1/chats/{id}/turns/{request_id}/retry")
        .unwrap();
    assert!(retry.request_body.is_none(), "retry has no body");
    let put = rig
        .openapi
        .op("PUT", "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction")
        .unwrap();
    assert!(put.request_body.is_some());
    let ok = put.responses.iter().find(|r| r.status == 200).unwrap();
    assert!(ok.content_type.starts_with("application/json"));
}

#[tokio::test]
async fn the_path_parameters_are_declared_and_every_route_needs_the_base_license() {
    let rig = send_rig().await;
    for (method, path, _, _, _) in expected() {
        let op = rig.openapi.op(method, path).unwrap();
        assert!(op.authenticated, "{path}");
        assert!(op.license_requirement.is_some(), "{path}");
        assert!(
            op.params.iter().any(|p| p.name == "id" && p.required),
            "{path}"
        );
        if path.contains("{request_id}") {
            assert!(
                op.params
                    .iter()
                    .any(|p| p.name == "request_id" && p.required)
            );
        }
        if path.contains("{msg_id}") {
            assert!(op.params.iter().any(|p| p.name == "msg_id" && p.required));
        }
    }
}

#[tokio::test]
async fn the_schemas_of_the_edit_and_reaction_bodies_are_declared() {
    let rig = send_rig().await;
    let document = rig
        .openapi
        .inner
        .build_openapi(&toolkit::api::OpenApiInfo::default())
        .unwrap();
    let value = serde_json::to_value(&document).unwrap();
    let schemas = &value["components"]["schemas"];
    let required = |name: &str| -> BTreeSet<String> {
        schemas[name]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} has no required list in {}", schemas[name]))
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        required("EditTurnRequest"),
        BTreeSet::from(["content".to_owned()])
    );
    assert_eq!(
        required("SetReactionReq"),
        BTreeSet::from(["reaction".to_owned()])
    );
    assert_eq!(
        required("MiniChatReactionDto"),
        BTreeSet::from([
            "message_id".to_owned(),
            "reaction".to_owned(),
            "created_at".to_owned()
        ])
    );
}

// ── the metrics through the endpoints ────────────────────────────────────────

async fn metered(upstream: Arc<MockProvider>) -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_clock(FixedClock::at(default_now()))
            .with_metrics(),
    )
    .await
}

fn quiet() -> Arc<MockProvider> {
    MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))))
}

fn counted(rig: &SendRig, op: &str, result: &str) -> i64 {
    rig.metrics
        .as_ref()
        .unwrap()
        .series("turn_mutation")
        .into_iter()
        .filter(|(a, _)| a["op"] == op && a["result"] == result)
        .map(|(_, v)| v)
        .sum()
}

fn latency_samples(rig: &SendRig, op: &str) -> i64 {
    rig.metrics
        .as_ref()
        .unwrap()
        .series("turn_mutation_latency_ms")
        .into_iter()
        .filter(|(a, _)| a["op"] == op)
        .map(|(_, v)| v)
        .sum()
}

#[tokio::test]
async fn a_committed_retry_edit_and_delete_each_record_ok_and_one_latency_sample() {
    // inst-met-counter, inst-met-latency, inst-obs-mutation-counter, inst-obs-mutation-latency
    let rig = metered(quiet()).await;
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("q").at(10)).await;
    let reply = rig.retry(chat.id, first.request_id).await;
    let (second, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, second).await;
    let reply = rig
        .edit(chat.id, second, &json!({ "content": "edited" }))
        .await;
    let (third, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, third).await;
    assert_eq!(rig.delete_turn(chat.id, third).await.status, 204);

    for op in ["retry", "edit", "delete"] {
        assert_eq!(counted(&rig, op, "ok"), 1, "{op}");
        assert_eq!(latency_samples(&rig, op), 1, "{op}");
    }
    for point in rig.metrics.as_ref().unwrap().series("turn_mutation") {
        let keys: BTreeSet<&str> = point.0.keys().map(String::as_str).collect();
        assert_eq!(keys, BTreeSet::from(["op", "result"]), "{:?}", point.0);
    }
    for point in rig
        .metrics
        .as_ref()
        .unwrap()
        .series("turn_mutation_latency_ms")
    {
        let keys: BTreeSet<&str> = point.0.keys().map(String::as_str).collect();
        assert_eq!(keys, BTreeSet::from(["op"]), "{:?}", point.0);
    }
}

#[tokio::test]
async fn every_rejection_class_is_counted_once_without_a_latency_sample() {
    // inst-met-counter: the rejection classes; inst-met-latency: committed mutations only
    let rig = metered(quiet()).await;
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    seed(&rig.quota.db, &chat, turn("second").at(100)).await;

    // not_latest_turn
    assert_eq!(rig.retry(chat.id, first.request_id).await.status, 409);
    assert_eq!(counted(&rig, "retry", "not_latest_turn"), 1);
    // not_found
    assert_eq!(rig.delete_turn(chat.id, Uuid::new_v4()).await.status, 404);
    assert_eq!(counted(&rig, "delete", "not_found"), 1);
    // preflight_rejected: the quota is gone
    rig.gateway.set_limits((1, 1), (1, 1));
    let latest = crate::test_support::send::turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .max_by_key(|row| row.started_at)
        .unwrap();
    assert_eq!(rig.retry(chat.id, latest.request_id).await.status, 429);
    assert_eq!(counted(&rig, "retry", "preflight_rejected"), 1);

    for op in ["retry", "edit", "delete"] {
        assert_eq!(latency_samples(&rig, op), 0, "{op}: no committed mutation");
        assert_eq!(counted(&rig, op, "ok"), 0, "{op}");
    }
}

#[tokio::test]
async fn forbidden_turn_running_generation_in_progress_and_error_are_counted() {
    let rig = metered(quiet()).await;
    // forbidden: a turn of another requester
    let chat = rig.std_chat().await;
    let foreign = seed(
        &rig.quota.db,
        &chat,
        turn("q").requester(Some(crate::test_support::USER_A2)),
    )
    .await;
    assert_eq!(rig.retry(chat.id, foreign.request_id).await.status, 403);
    assert_eq!(counted(&rig, "retry", "forbidden"), 1);

    // turn_running
    let chat = rig.std_chat().await;
    let running = seed(&rig.quota.db, &chat, turn("q").running()).await;
    assert_eq!(
        rig.edit(chat.id, running.request_id, &json!({ "content": "x" }))
            .await
            .status,
        400
    );
    assert_eq!(counted(&rig, "edit", "turn_running"), 1);

    // generation_in_progress: an older running turn holds the index
    let chat = rig.std_chat().await;
    seed(&rig.quota.db, &chat, turn("stuck").at(10).running()).await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(100)).await;
    assert_eq!(rig.retry(chat.id, target.request_id).await.status, 409);
    assert_eq!(counted(&rig, "retry", "generation_in_progress"), 1);

    // error: the audit enqueue fails
    let outbox = FailingMutationOutbox::internal();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_turn_outbox(outbox.clone())
            .with_metrics(),
    )
    .await;
    outbox.bind(rig.quota.enqueuer.clone());
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.delete_turn(chat.id, target.request_id).await;
    assert_internal(&reply);
    assert_eq!(counted(&rig, "delete", "error"), 1);
    assert_eq!(latency_samples(&rig, "delete"), 0);
}

#[tokio::test]
async fn a_later_setup_failure_is_reported_through_the_turn_state_not_through_the_counter() {
    // inst-met-counter: `ok` when the mutation committed
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(quiet())
            .with_snapshot(crate::test_support::send::catalog_of(vec![
                crate::test_support::send::prem_model(&json!({})),
                crate::test_support::send::std_model(&json!({ "provider_id": "ghost_provider" })),
            ]))
            .with_metrics(),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_eq!(reply.status, 500, "{}", reply.text);
    assert_eq!(counted(&rig, "retry", "ok"), 1);
    assert_eq!(latency_samples(&rig, "retry"), 1);
    assert_eq!(counted(&rig, "retry", "error"), 0);
}

#[tokio::test]
async fn reactions_record_no_metric() {
    // inst-met-no-reaction-metric
    let rig = send_rig_with(SendRigOptions::new().with_metrics()).await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let assistant = seeded.assistant_message_id.unwrap();
    assert_eq!(
        rig.put_reaction(chat.id, assistant, "like").await.status,
        200
    );
    assert_eq!(rig.delete_reaction(chat.id, assistant).await.status, 204);
    let metrics = rig.metrics.as_ref().unwrap();
    assert_eq!(metrics.total("turn_mutation"), 0);
    assert_eq!(metrics.total("turn_mutation_latency_ms"), 0);
    assert!(
        metrics
            .recorded()
            .iter()
            .all(|name| !name.contains("reaction")),
        "{:?}",
        metrics.recorded()
    );
}

// ── the deferred scope ───────────────────────────────────────────────────────

#[tokio::test]
async fn no_older_turn_can_be_mutated_and_no_branch_is_created() {
    // inst-mp-not-latest: tail-only mutations, a linear conversation
    let (rig, chat, _upstream) = streaming_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    assert_eq!(rig.retry(chat.id, first.request_id).await.status, 409);
    assert_eq!(
        rig.edit(chat.id, first.request_id, &json!({ "content": "x" }))
            .await
            .status,
        409
    );
    assert_eq!(rig.delete_turn(chat.id, first.request_id).await.status, 409);
    let live = turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .filter(|row| row.deleted_at.is_none())
        .count();
    assert_eq!(live, 2, "no branch, no extra turn");
}

#[tokio::test]
async fn the_client_cannot_choose_the_request_id_of_a_retry() {
    // the retry endpoint has no body: a supplied one is ignored
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let wanted = Uuid::new_v4();
    let reply = rig
        .send(crate::test_support::chats::json_request(
            "POST",
            &crate::test_support::mutations::retry_uri(chat.id, target.request_id),
            &a1(),
            &json!({ "request_id": wanted.to_string() }).to_string(),
        ))
        .await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    assert_ne!(new_request, wanted);
}

#[tokio::test]
async fn soft_deleted_rows_are_never_restored_or_purged() {
    // inst-tv-final: no restore endpoint, the rows stay in storage
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let rows_before = message_rows(&rig.quota.db, chat.id).await.len();
    assert_eq!(
        rig.delete_turn(chat.id, target.request_id).await.status,
        204
    );
    assert_eq!(
        message_rows(&rig.quota.db, chat.id).await.len(),
        rows_before,
        "no purge"
    );
    for (method, uri) in [
        (
            "POST",
            format!("{}/restore", turn_uri(chat.id, target.request_id)),
        ),
        ("POST", turn_uri(chat.id, target.request_id)),
        ("PUT", turn_uri(chat.id, target.request_id)),
        (
            "PATCH",
            format!("{}/restore", turn_uri(chat.id, target.request_id)),
        ),
        ("POST", format!("/mini-chat/v1/chats/{}/turns", chat.id)),
    ] {
        let reply = rig.send(request(method, &uri, &a1())).await;
        assert!(
            reply.status == 404 || reply.status == 405,
            "{method} {uri}: {} {}",
            reply.status,
            reply.text
        );
    }
    // still deleted: a second delete is a conflict, not a success
    assert_eq!(
        rig.delete_turn(chat.id, target.request_id).await.status,
        409
    );
}

#[tokio::test]
async fn there_is_no_endpoint_to_mutate_or_react_to_other_resources() {
    // no message delete or edit, no reaction list, no reaction on a chat
    let (rig, chat, _upstream) = streaming_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let message = seeded.assistant_message_id.unwrap();
    for (method, uri) in [
        (
            "DELETE",
            format!("/mini-chat/v1/chats/{}/messages/{message}", chat.id),
        ),
        (
            "PATCH",
            format!("/mini-chat/v1/chats/{}/messages/{message}", chat.id),
        ),
        (
            "GET",
            format!(
                "/mini-chat/v1/chats/{}/messages/{message}/reaction",
                chat.id
            ),
        ),
        ("PUT", format!("/mini-chat/v1/chats/{}/reaction", chat.id)),
    ] {
        let reply = rig.send(request(method, &uri, &a1())).await;
        assert!(
            reply.status == 404 || reply.status == 405,
            "{method} {uri}: {} {}",
            reply.status,
            reply.text
        );
    }
}

#[tokio::test]
async fn the_messages_list_exposes_no_deletion_or_replacement_data() {
    // soft-deleted rows are retained for audit but never part of the API
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;
    for item in rig.list_messages(chat.id).await {
        for forbidden in ["deleted_at", "replaced_by_request_id", "is_compressed"] {
            assert!(item.get(forbidden).is_none(), "{forbidden} in {item}");
        }
    }
    let status = rig.get(&turn_uri(chat.id, new_request), &a1()).await;
    let body = status.json();
    for forbidden in ["deleted_at", "replaced_by_request_id", "chat_id"] {
        assert!(body.get(forbidden).is_none(), "{forbidden} in {body}");
    }
}
