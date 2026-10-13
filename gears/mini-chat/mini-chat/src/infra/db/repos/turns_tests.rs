#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::integer_division,
    clippy::unused_self,
    clippy::useless_format,
    clippy::type_complexity
)]
//! Slice 2, 3, 5 and 6: the turn store on a real (SQLite) database: the
//! pre-reserve reads, the reserve transaction with its rollbacks and unique
//! violations, the finalization transaction with its writes and outbox rows,
//! the liveness query and the progress writes.

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatAuditEvent, TerminalState, TurnAuditEvent, TurnAuditEventType, UsageTokens,
    decode_audit_event,
};
use time::{Duration, OffsetDateTime};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::chat_test_env::{BASE_UNIX, att_id, blob, msg_id, request_id, ts};
use crate::config::OutboxConfig;
use crate::domain::service::quota::finalize::{
    FinalizeVariant, TerminalClassification, TerminalColumns,
};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::preflight::PreflightDecision;
use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest};
use crate::domain::service::quota::settle::ToolCallCounts;
use crate::domain::service::quota::test_support::{noon, premium_decision};
use crate::domain::service::stream::ports::{AssistantMessageWrite, AttachmentKind, HistoryRole};
use crate::infra::db::test_support::{Row, attachment_row, message_row, turn_row};
use crate::infra::outbox::queues::OutboxEnqueuer;
use crate::quota_test_env::QuotaEnv;
use crate::test_doubles::{OTHER_TENANT, USER, USER_TENANT};

const AUDIT_QUEUE: &str = "mini-chat.audit";

fn scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT).ensure_owner(USER)
}

/// Six fraction digits with a non-zero last digit: the stored text of this
/// instant and of the same instant plus one microsecond sort chronologically
/// (see `stream_test_env::base_time`).
fn now() -> OffsetDateTime {
    noon() + Duration::microseconds(250_001)
}

fn at_unix(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(BASE_UNIX + secs).unwrap()
}

struct Fx {
    env: QuotaEnv,
    repo: SeaTurnRepository,
    chat: Uuid,
    decision: PreflightDecision,
}

impl Fx {
    async fn new() -> Self {
        Self::on(QuotaEnv::new().await).await
    }

    async fn on(env: QuotaEnv) -> Self {
        let chat = env.seed_chat(1).await;
        let repo = SeaTurnRepository::new(
            Arc::clone(&env.provider),
            Arc::clone(&env.repo),
            Arc::clone(&env.enqueuer),
        );
        Self {
            env,
            repo,
            chat,
            decision: premium_decision(),
        }
    }

    fn input<'a>(&'a self, n: u128, ids: &'a [Uuid]) -> CreateTurnInput<'a> {
        CreateTurnInput {
            reserve: ReserveRequest {
                tenant_id: USER_TENANT,
                user_id: USER,
                mode: ReserveMode::Send,
                decision: &self.decision,
                now: now(),
            },
            chat_id: self.chat,
            request_id: request_id(n),
            turn_id: Uuid::from_u128(0xA000_0000 + n),
            user_message_id: msg_id(n),
            content: "  the question \n",
            attachment_ids: ids,
            web_search_enabled: true,
        }
    }

    async fn create(&self, n: u128) -> Result<ReservedColumns, DomainError> {
        self.repo.create_turn(&self.input(n, &[])).await
    }

    fn turn(&self, n: u128) -> Uuid {
        Uuid::from_u128(0xA000_0000 + n)
    }

    /// `quote(column)` of the row `id`; blobs are returned with lower-case
    /// hex like [`blob`].
    async fn col(&self, table: &str, id: Uuid, column: &str) -> String {
        let quoted = self
            .env
            .db
            .query_string(&format!(
                "SELECT quote({column}) FROM {table} WHERE id = {}",
                blob(id)
            ))
            .await;
        if quoted.starts_with("X'") {
            quoted.to_lowercase().replacen('x', "X", 1)
        } else {
            quoted
        }
    }

    /// A time column of the row `id` in microseconds since the epoch.
    async fn at(&self, table: &str, id: Uuid, column: &str) -> i128 {
        micros(&self.col(table, id, column).await)
    }

    async fn count(&self, table: &str) -> i64 {
        self.env.db.count(table).await
    }

    async fn snapshot_of_everything(&self) -> String {
        let mut out = String::new();
        for table in [
            "chats",
            "messages",
            "chat_turns",
            "message_attachments",
            "quota_usage",
        ] {
            out.push_str(&self.env.dump(table).await);
            out.push('\n');
        }
        out
    }

    async fn seed_attachment(&self, n: u128, edit: impl FnOnce(Row) -> Row) -> Uuid {
        let id = att_id(n);
        let row = edit(
            attachment_row(id, USER_TENANT, self.chat)
                .set("uploaded_by_user_id", USER)
                .set("created_at", ts(0))
                .set("updated_at", ts(0)),
        );
        self.env.db.insert("attachments", row).await.unwrap();
        id
    }

    fn completed_request(&self, n: u128, usage: Option<UsageTokens>) -> FinalizeRequest {
        self.request(
            n,
            TerminalClassification::Completed,
            TerminalColumns {
                state: TerminalState::Completed,
                error_code: None,
                error_detail: None,
                assistant_message_id: Some(assistant_id(n)),
                provider_response_id: Some("resp_internal".to_owned()),
            },
            usage,
        )
    }

    fn request(
        &self,
        n: u128,
        classification: TerminalClassification,
        columns: TerminalColumns,
        usage: Option<UsageTokens>,
    ) -> FinalizeRequest {
        FinalizeRequest {
            turn_id: self.turn(n),
            variant: FinalizeVariant::Stream {
                tenant_id: USER_TENANT,
                period_keys: self.decision.period_keys,
                selected_model: "p".to_owned(),
            },
            classification,
            columns,
            usage,
            tool_calls: ToolCallCounts {
                web_search: 1,
                code_interpreter: 0,
                file_search: 2,
            },
            snapshot: SettlementSnapshot::Available(Box::new(self.decision.snapshot.clone())),
            now: now() + Duration::seconds(3),
        }
    }

    fn writes(&self, n: u128, content: Option<&str>) -> FinalizeWrites {
        FinalizeWrites {
            assistant_message: content.map(|text| AssistantMessageWrite {
                id: assistant_id(n),
                tenant_id: USER_TENANT,
                chat_id: self.chat,
                request_id: request_id(n),
                content: text.to_owned(),
                provider_response_id: Some("resp_internal".to_owned()),
                usage: Some(UsageTokens {
                    input_tokens: 120,
                    output_tokens: 34,
                    cache_read_input_tokens: 20,
                    cache_write_input_tokens: 5,
                    reasoning_tokens: 7,
                }),
                model: "p".to_owned(),
                created_at: now() + Duration::seconds(3),
            }),
            audit: self.audit(n),
            summary: None,
        }
    }

    fn audit(&self, n: u128) -> TurnAuditEvent {
        TurnAuditEvent {
            event_type: TurnAuditEventType::TurnCompleted,
            tenant_id: USER_TENANT,
            requester_user_id: USER,
            chat_id: self.chat,
            turn_id: self.turn(n),
            request_id: request_id(n),
            selected_model: "p".to_owned(),
            effective_model: "p".to_owned(),
            usage: mini_chat_sdk::AuditUsage {
                input_tokens: 120,
                output_tokens: 34,
            },
            latency_ms: 1500,
            tool_calls: mini_chat_sdk::AuditToolCalls {
                web_search: 1,
                file_search: 2,
            },
            policy_decisions: mini_chat_sdk::AuditPolicyDecisions {
                quota: mini_chat_sdk::AuditQuotaDecision {
                    decision: "allow".to_owned(),
                },
            },
            timestamp: now() + Duration::seconds(3),
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            license: None,
            quota_scope: None,
        }
    }
}

/// Microseconds since the epoch of a stored timestamp.
fn micros(stored: &str) -> i128 {
    i128::from(crate::chat_test_env::parse_ts(stored).timestamp_micros())
}

fn micros_of(at: OffsetDateTime) -> i128 {
    at.unix_timestamp_nanos() / 1000
}

fn assistant_id(n: u128) -> Uuid {
    Uuid::from_u128(0xB000_0000 + n)
}

fn usage(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

async fn seed_turn(fx: &Fx, n: u128, state: &str, edit: impl FnOnce(Row) -> Row) {
    fx.env
        .db
        .insert(
            "chat_turns",
            edit(turn_row(
                fx.turn(n),
                USER_TENANT,
                fx.chat,
                request_id(n),
                state,
            )),
        )
        .await
        .unwrap();
}

async fn seed_message(fx: &Fx, n: u128, edit: impl FnOnce(Row) -> Row) -> Uuid {
    fx.env
        .db
        .insert(
            "messages",
            edit(message_row(msg_id(n), USER_TENANT, fx.chat)),
        )
        .await
        .unwrap();
    msg_id(n)
}

// ==== reads =====================================================================================

#[tokio::test]
async fn a_turn_is_found_by_its_request_id_with_every_projected_column() {
    let fx = Fx::new().await;
    seed_turn(&fx, 1, "completed", |row| {
        row.set("assistant_message_id", assistant_id(1))
            .set("error_code", "x")
            .set("effective_model", "p")
            .set("started_at", ts(10))
            .set("updated_at", ts(20))
    })
    .await;
    let found = fx
        .repo
        .find_by_request_id(&scope(), fx.chat, request_id(1))
        .await
        .unwrap()
        .expect("the turn");
    assert_eq!(found.id, fx.turn(1));
    assert_eq!(found.tenant_id, USER_TENANT);
    assert_eq!(found.chat_id, fx.chat);
    assert_eq!(found.request_id, request_id(1));
    assert_eq!(found.state, TurnState::Completed);
    assert_eq!(found.assistant_message_id, Some(assistant_id(1)));
    assert_eq!(found.error_code.as_deref(), Some("x"));
    assert_eq!(found.effective_model.as_deref(), Some("p"));
    assert_eq!(found.started_at, at_unix(10));
    assert_eq!(found.updated_at, at_unix(20));
    assert_eq!(found.requester_user_id, Some(Uuid::from_u128(0xAA)));
    assert!(!found.deleted);
}

#[tokio::test]
async fn a_soft_deleted_turn_is_found_and_flagged() {
    let fx = Fx::new().await;
    seed_turn(&fx, 1, "completed", |row| row).await;
    fx.env.db.soft_delete("chat_turns", fx.turn(1)).await;
    let found = fx
        .repo
        .find_by_request_id(&scope(), fx.chat, request_id(1))
        .await
        .unwrap()
        .unwrap();
    assert!(
        found.deleted,
        "the key stays taken by UNIQUE (chat_id, request_id)"
    );
}

#[tokio::test]
async fn the_request_id_lookup_is_bound_to_the_chat_and_the_tenant() {
    let fx = Fx::new().await;
    seed_turn(&fx, 1, "completed", |row| row).await;
    let other_chat = fx.env.seed_chat(2).await;
    assert!(
        fx.repo
            .find_by_request_id(&scope(), other_chat, request_id(1))
            .await
            .unwrap()
            .is_none()
    );
    let foreign = AccessScope::for_tenant(OTHER_TENANT);
    assert!(
        fx.repo
            .find_by_request_id(&foreign, fx.chat, request_id(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fx.repo
            .find_by_request_id(&scope(), fx.chat, request_id(99))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn only_a_live_running_turn_of_the_chat_counts_as_running() {
    let fx = Fx::new().await;
    assert!(
        fx.repo
            .find_running(&scope(), fx.chat)
            .await
            .unwrap()
            .is_none()
    );
    seed_turn(&fx, 1, "completed", |row| row).await;
    seed_turn(&fx, 2, "failed", |row| row).await;
    seed_turn(&fx, 3, "cancelled", |row| row).await;
    assert!(
        fx.repo
            .find_running(&scope(), fx.chat)
            .await
            .unwrap()
            .is_none()
    );
    seed_turn(&fx, 4, "running", |row| row).await;
    fx.env.db.soft_delete("chat_turns", fx.turn(4)).await;
    assert!(
        fx.repo
            .find_running(&scope(), fx.chat)
            .await
            .unwrap()
            .is_none(),
        "a soft-deleted running turn does not block"
    );
    seed_turn(&fx, 5, "running", |row| row).await;
    let running = fx
        .repo
        .find_running(&scope(), fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.id, fx.turn(5));
    assert_eq!(running.state, TurnState::Running);
    let other_chat = fx.env.seed_chat(2).await;
    assert!(
        fx.repo
            .find_running(&scope(), other_chat)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_assistant_message_of_a_turn_is_loaded_within_its_chat() {
    let fx = Fx::new().await;
    seed_message(&fx, 1, |row| {
        row.set("role", "assistant")
            .set("content", "the answer")
            .set("input_tokens", 12_i64)
            .set("output_tokens", 5_i64)
    })
    .await;
    let loaded = fx
        .repo
        .load_assistant_message(&scope(), fx.chat, msg_id(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.id, msg_id(1));
    assert_eq!(loaded.content, "the answer");
    assert_eq!((loaded.input_tokens, loaded.output_tokens), (12, 5));
}

#[tokio::test]
async fn a_deleted_or_foreign_assistant_message_is_not_loaded() {
    let fx = Fx::new().await;
    seed_message(&fx, 1, |row| row.set("role", "assistant")).await;
    seed_message(&fx, 2, |row| row.set("role", "assistant")).await;
    fx.env.db.soft_delete("messages", msg_id(2)).await;
    let other_chat = fx.env.seed_chat(2).await;
    assert!(
        fx.repo
            .load_assistant_message(&scope(), fx.chat, msg_id(2))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fx.repo
            .load_assistant_message(&scope(), other_chat, msg_id(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fx.repo
            .load_assistant_message(&AccessScope::for_tenant(OTHER_TENANT), fx.chat, msg_id(1))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn request_attachments_are_the_live_rows_of_the_chat_among_the_ids() {
    let fx = Fx::new().await;
    let doc = fx.seed_attachment(1, |row| row).await;
    let img = fx
        .seed_attachment(2, |row| {
            row.set("attachment_kind", "image")
                .set("provider_file_id", "file-img")
                .set("secondary_file_id", "sec-img")
                .set("status", "uploaded")
                .set("filename", "pic.png")
        })
        .await;
    let deleted = fx.seed_attachment(3, |row| row).await;
    fx.env.db.soft_delete("attachments", deleted).await;
    let other_chat = fx.env.seed_chat(2).await;
    let foreign = att_id(4);
    fx.env
        .db
        .insert(
            "attachments",
            attachment_row(foreign, USER_TENANT, other_chat),
        )
        .await
        .unwrap();
    let rows = fx
        .repo
        .request_attachments(&scope(), fx.chat, &[doc, img, deleted, foreign, att_id(99)])
        .await
        .unwrap();
    let mut ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    ids.sort();
    assert_eq!(ids, vec![doc, img], "no deleted, foreign or unknown id");
    let image = rows.iter().find(|r| r.id == img).unwrap();
    assert_eq!(image.kind, AttachmentKind::Image);
    assert_eq!(image.status, "uploaded", "any status is returned");
    assert_eq!(image.provider_file_id.as_deref(), Some("file-img"));
    assert_eq!(image.secondary_file_id.as_deref(), Some("sec-img"));
    assert_eq!(image.filename, "pic.png");
    let document = rows.iter().find(|r| r.id == doc).unwrap();
    assert_eq!(document.kind, AttachmentKind::Document);
}

#[tokio::test]
async fn ready_attachments_are_the_live_ready_rows_of_the_chat() {
    let fx = Fx::new().await;
    fx.seed_attachment(1, |row| row.set("provider_file_id", "file-1"))
        .await;
    fx.seed_attachment(2, |row| row.set("status", "pending"))
        .await;
    fx.seed_attachment(3, |row| row.set("status", "failed"))
        .await;
    let gone = fx.seed_attachment(4, |row| row).await;
    fx.env.db.soft_delete("attachments", gone).await;
    fx.seed_attachment(5, |row| {
        row.set("for_file_search", false)
            .set("for_code_interpreter", true)
            .set("provider_file_id", "file-5")
    })
    .await;
    let mut rows = fx.repo.ready_attachments(&scope(), fx.chat).await.unwrap();
    rows.sort_by_key(|r| r.id);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].for_file_search && !rows[0].for_code_interpreter);
    assert_eq!(rows[0].provider_file_id.as_deref(), Some("file-1"));
    assert!(!rows[1].for_file_search && rows[1].for_code_interpreter);
}

#[tokio::test]
async fn the_vector_store_id_is_only_read_through_the_chat_and_the_tenant() {
    let fx = Fx::new().await;
    assert_eq!(
        fx.repo.vector_store_id(&scope(), fx.chat).await.unwrap(),
        None
    );
    fx.env
        .db
        .insert(
            "chat_vector_stores",
            Row::default()
                .set("id", Uuid::from_u128(0x5001))
                .set("tenant_id", USER_TENANT)
                .set("chat_id", fx.chat)
                .set("vector_store_id", "vs_chat")
                .set("provider", "openai")
                .set("file_count", 1_i32)
                .set("created_at", ts(0)),
        )
        .await
        .unwrap();
    assert_eq!(
        fx.repo
            .vector_store_id(&scope(), fx.chat)
            .await
            .unwrap()
            .as_deref(),
        Some("vs_chat")
    );
    let other_chat = fx.env.seed_chat(2).await;
    assert_eq!(
        fx.repo.vector_store_id(&scope(), other_chat).await.unwrap(),
        None
    );
    assert_eq!(
        fx.repo
            .vector_store_id(&AccessScope::for_tenant(OTHER_TENANT), fx.chat)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn a_vector_store_row_without_an_id_yields_none() {
    let fx = Fx::new().await;
    fx.env
        .db
        .insert(
            "chat_vector_stores",
            Row::default()
                .set("id", Uuid::from_u128(0x5001))
                .set("tenant_id", USER_TENANT)
                .set("chat_id", fx.chat)
                .null("vector_store_id")
                .set("provider", "openai")
                .set("file_count", 0_i32)
                .set("created_at", ts(0)),
        )
        .await
        .unwrap();
    assert_eq!(
        fx.repo.vector_store_id(&scope(), fx.chat).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn the_snapshot_boundary_is_the_latest_live_message_with_a_request_id() {
    let fx = Fx::new().await;
    assert_eq!(
        fx.repo.snapshot_boundary(&scope(), fx.chat).await.unwrap(),
        None
    );
    seed_message(&fx, 1, |r| {
        r.set("request_id", request_id(1)).set("created_at", ts(10))
    })
    .await;
    seed_message(&fx, 2, |r| {
        r.set("request_id", request_id(2)).set("created_at", ts(20))
    })
    .await;
    // Newer but without a request id, and newer but deleted: both ignored.
    seed_message(&fx, 3, |r| r.set("created_at", ts(30))).await;
    seed_message(&fx, 4, |r| {
        r.set("request_id", request_id(4)).set("created_at", ts(40))
    })
    .await;
    fx.env.db.soft_delete("messages", msg_id(4)).await;
    let boundary = fx
        .repo
        .snapshot_boundary(&scope(), fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(boundary.id, msg_id(2));
    assert_eq!(boundary.created_at, at_unix(20));
}

#[tokio::test]
async fn equal_timestamps_are_ordered_by_id_descending_at_the_boundary() {
    let fx = Fx::new().await;
    seed_message(&fx, 1, |r| {
        r.set("request_id", request_id(1)).set("created_at", ts(10))
    })
    .await;
    seed_message(&fx, 2, |r| {
        r.set("request_id", request_id(2)).set("created_at", ts(10))
    })
    .await;
    let boundary = fx
        .repo
        .snapshot_boundary(&scope(), fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(boundary.id, msg_id(2));
}

#[tokio::test]
async fn the_thread_summary_is_read_with_its_frontier_and_estimate() {
    let fx = Fx::new().await;
    assert!(
        fx.repo
            .thread_summary(&scope(), fx.chat)
            .await
            .unwrap()
            .is_none()
    );
    fx.env
        .db
        .insert(
            "thread_summaries",
            Row::default()
                .set("id", Uuid::from_u128(0x6001))
                .set("tenant_id", USER_TENANT)
                .set("chat_id", fx.chat)
                .set("summary_text", "earlier talk")
                .set("summarized_up_to_created_at", ts(5))
                .set("summarized_up_to_message_id", msg_id(3))
                .set("token_estimate", 42_i32)
                .set("created_at", ts(6))
                .set("updated_at", ts(6)),
        )
        .await
        .unwrap();
    let summary = fx
        .repo
        .thread_summary(&scope(), fx.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.summary_text, "earlier talk");
    assert_eq!(summary.frontier.created_at, at_unix(5));
    assert_eq!(summary.frontier.id, msg_id(3));
    assert_eq!(summary.token_estimate, 42);
    assert!(
        fx.repo
            .thread_summary(&AccessScope::for_tenant(OTHER_TENANT), fx.chat)
            .await
            .unwrap()
            .is_none()
    );
}

fn boundary(secs: i64, n: u128) -> MessageBoundary {
    MessageBoundary {
        created_at: at_unix(secs),
        id: msg_id(n),
    }
}

async fn seed_history(fx: &Fx) {
    for (n, role, secs) in [
        (1, "user", 10),
        (2, "assistant", 11),
        (3, "user", 20),
        (4, "assistant", 21),
        (5, "user", 30),
        (6, "assistant", 31),
    ] {
        seed_message(fx, n, |r| {
            r.set("role", role)
                .set("request_id", request_id(n))
                .set("created_at", ts(secs))
                .set("content", format!("m{n}"))
        })
        .await;
    }
}

fn contents(rows: &[crate::domain::service::stream::ports::HistoryMessage]) -> Vec<String> {
    rows.iter().map(|m| m.content.clone()).collect()
}

#[tokio::test]
async fn recent_messages_are_the_newest_rows_up_to_the_boundary_in_chronological_order() {
    let fx = Fx::new().await;
    seed_history(&fx).await;
    let rows = fx
        .repo
        .recent_messages(
            &scope(),
            fx.chat,
            &RecentQuery {
                boundary: boundary(31, 6),
                frontier: None,
                limit: 4,
            },
        )
        .await
        .unwrap();
    assert_eq!(contents(&rows), ["m3", "m4", "m5", "m6"]);
    assert_eq!(rows[0].role, HistoryRole::User);
    assert_eq!(rows[1].role, HistoryRole::Assistant);
    assert_eq!(rows[3].created_at, at_unix(31));
}

#[tokio::test]
async fn messages_after_the_boundary_are_excluded() {
    let fx = Fx::new().await;
    seed_history(&fx).await;
    let rows = fx
        .repo
        .recent_messages(
            &scope(),
            fx.chat,
            &RecentQuery {
                boundary: boundary(21, 4),
                frontier: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(contents(&rows), ["m1", "m2", "m3", "m4"]);
}

#[tokio::test]
async fn the_message_at_the_boundary_is_included_and_ties_are_broken_by_id() {
    let fx = Fx::new().await;
    seed_message(&fx, 1, |r| {
        r.set("request_id", request_id(1))
            .set("created_at", ts(10))
            .set("content", "a")
    })
    .await;
    seed_message(&fx, 2, |r| {
        r.set("request_id", request_id(2))
            .set("created_at", ts(10))
            .set("content", "b")
    })
    .await;
    seed_message(&fx, 3, |r| {
        r.set("request_id", request_id(3))
            .set("created_at", ts(10))
            .set("content", "c")
    })
    .await;
    let rows = fx
        .repo
        .recent_messages(
            &scope(),
            fx.chat,
            &RecentQuery {
                boundary: boundary(10, 2),
                frontier: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        contents(&rows),
        ["a", "b"],
        "(created_at, id) at most the boundary"
    );
}

#[tokio::test]
async fn the_summary_frontier_is_exclusive() {
    let fx = Fx::new().await;
    seed_history(&fx).await;
    let rows = fx
        .repo
        .recent_messages(
            &scope(),
            fx.chat,
            &RecentQuery {
                boundary: boundary(31, 6),
                frontier: Some(boundary(21, 4)),
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(contents(&rows), ["m5", "m6"], "strictly after the frontier");
}

#[tokio::test]
async fn compressed_deleted_system_and_request_less_messages_are_not_recent_history() {
    let fx = Fx::new().await;
    seed_message(&fx, 1, |r| {
        r.set("request_id", request_id(1))
            .set("created_at", ts(1))
            .set("content", "ok")
    })
    .await;
    seed_message(&fx, 2, |r| {
        r.set("request_id", request_id(2))
            .set("created_at", ts(2))
            .set("is_compressed", true)
    })
    .await;
    seed_message(&fx, 3, |r| {
        r.set("request_id", request_id(3)).set("created_at", ts(3))
    })
    .await;
    fx.env.db.soft_delete("messages", msg_id(3)).await;
    seed_message(&fx, 4, |r| {
        r.set("request_id", request_id(4))
            .set("created_at", ts(4))
            .set("role", "system")
    })
    .await;
    seed_message(&fx, 5, |r| r.set("created_at", ts(5))).await;
    let rows = fx
        .repo
        .recent_messages(
            &scope(),
            fx.chat,
            &RecentQuery {
                boundary: boundary(9, 9),
                frontier: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(contents(&rows), ["ok"]);
}

#[tokio::test]
async fn recent_messages_are_bound_to_the_chat_and_the_tenant() {
    let fx = Fx::new().await;
    seed_history(&fx).await;
    let other_chat = fx.env.seed_chat(2).await;
    let query = RecentQuery {
        boundary: boundary(31, 6),
        frontier: None,
        limit: 10,
    };
    assert!(
        fx.repo
            .recent_messages(&scope(), other_chat, &query)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fx.repo
            .recent_messages(&AccessScope::for_tenant(OTHER_TENANT), fx.chat, &query)
            .await
            .unwrap()
            .is_empty()
    );
}

// ==== the reserve transaction ===================================================================

#[tokio::test]
async fn the_reserve_writes_the_user_message_exactly() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let id = msg_id(1);
    for (column, expected) in [
        ("tenant_id", format!("{}", blob(USER_TENANT))),
        ("chat_id", blob(fx.chat)),
        ("request_id", blob(request_id(1))),
        ("role", "'user'".to_owned()),
        ("content", "'  the question \n'".to_owned()),
        ("content_type", "'text'".to_owned()),
        ("token_estimate", "0".to_owned()),
        ("provider_response_id", "NULL".to_owned()),
        ("request_kind", "'chat'".to_owned()),
        ("features_used", "'[]'".to_owned()),
        ("input_tokens", "0".to_owned()),
        ("output_tokens", "0".to_owned()),
        ("cache_read_input_tokens", "0".to_owned()),
        ("cache_write_input_tokens", "0".to_owned()),
        ("reasoning_tokens", "0".to_owned()),
        ("model", "NULL".to_owned()),
        ("is_compressed", "0".to_owned()),
        ("deleted_at", "NULL".to_owned()),
    ] {
        assert_eq!(
            fx.col("messages", id, column).await,
            expected,
            "messages.{column}"
        );
    }
    assert_eq!(fx.at("messages", id, "created_at").await, micros_of(now()));
}

#[tokio::test]
async fn the_reserve_writes_the_running_turn_with_the_exact_fields() {
    let fx = Fx::new().await;
    let columns = fx.create(1).await.unwrap();
    let id = fx.turn(1);
    for (column, expected) in [
        ("tenant_id", blob(USER_TENANT)),
        ("chat_id", blob(fx.chat)),
        ("request_id", blob(request_id(1))),
        ("requester_type", "'user'".to_owned()),
        ("requester_user_id", blob(USER)),
        ("state", "'running'".to_owned()),
        ("provider_name", "NULL".to_owned()),
        ("provider_response_id", "NULL".to_owned()),
        ("assistant_message_id", "NULL".to_owned()),
        ("error_code", "NULL".to_owned()),
        ("error_detail", "NULL".to_owned()),
        ("reserve_tokens", columns.reserve_tokens.to_string()),
        (
            "max_output_tokens_applied",
            columns.max_output_tokens_applied.to_string(),
        ),
        (
            "reserved_credits_micro",
            columns.reserved_credits_micro.to_string(),
        ),
        ("policy_version_applied", "7".to_owned()),
        ("effective_model", "'p'".to_owned()),
        (
            "minimal_generation_floor_applied",
            columns.minimal_generation_floor_applied.to_string(),
        ),
        ("web_search_enabled", "1".to_owned()),
        ("web_search_completed_count", "0".to_owned()),
        ("code_interpreter_completed_count", "0".to_owned()),
        ("file_search_completed_count", "0".to_owned()),
        ("completed_at", "NULL".to_owned()),
        ("deleted_at", "NULL".to_owned()),
    ] {
        assert_eq!(
            fx.col("chat_turns", id, column).await,
            expected,
            "chat_turns.{column}"
        );
    }
    for column in ["started_at", "last_progress_at", "updated_at"] {
        assert_eq!(
            fx.at("chat_turns", id, column).await,
            micros_of(now()),
            "{column}"
        );
    }
}

#[tokio::test]
async fn the_reserve_returns_the_six_preflight_columns_of_the_decision() {
    let fx = Fx::new().await;
    let columns = fx.create(1).await.unwrap();
    assert_eq!(columns, ReservedColumns::from_decision(&fx.decision));
}

#[tokio::test]
async fn the_reserve_bumps_the_chat_and_books_the_quota_in_the_same_transaction() {
    let fx = Fx::new().await;
    let before = fx.col("chats", fx.chat, "updated_at").await;
    fx.create(1).await.unwrap();
    let after = fx.col("chats", fx.chat, "updated_at").await;
    assert_ne!(before, after);
    assert_eq!(micros(&after), micros_of(now()));
    for period in ["daily", "monthly"] {
        assert_eq!(
            fx.env.total(period).await.unwrap().reserved,
            fx.decision.reserve.reserved_credits_micro
        );
        assert_eq!(
            fx.env.premium(period).await.unwrap().reserved,
            fx.decision.reserve.reserved_credits_micro,
            "a premium turn also reserves in tier:premium"
        );
    }
}

#[tokio::test]
async fn attachment_links_are_written_with_the_ids_of_the_message_and_the_chat() {
    let fx = Fx::new().await;
    let a = fx.seed_attachment(1, |row| row).await;
    let b = fx
        .seed_attachment(2, |row| row.set("attachment_kind", "image"))
        .await;
    fx.repo.create_turn(&fx.input(1, &[a, b])).await.unwrap();
    assert_eq!(fx.count("message_attachments").await, 2);
    let linked = fx
        .env
        .db
        .query_i64(&format!(
            "SELECT COUNT(*) FROM message_attachments WHERE tenant_id = {} AND chat_id = {} \
             AND message_id = {}",
            blob(USER_TENANT),
            blob(fx.chat),
            blob(msg_id(1))
        ))
        .await;
    assert_eq!(linked, 2);
    let stamp = fx
        .env
        .db
        .query_string("SELECT quote(created_at) FROM message_attachments LIMIT 1")
        .await;
    assert_eq!(micros(&stamp), micros_of(now()));
}

async fn assert_rolled_back(fx: &Fx, before: &str) {
    assert_eq!(
        &fx.snapshot_of_everything().await,
        before,
        "nothing is left behind"
    );
    assert_eq!(fx.env.outbox_message_count().await, 0);
}

#[tokio::test]
async fn an_invalid_attachment_rolls_back_the_reserve_the_message_and_the_chat_bump() {
    let cases: Vec<(&str, Box<dyn Fn(Row) -> Row>)> = vec![
        ("pending", Box::new(|row: Row| row.set("status", "pending"))),
        (
            "uploaded",
            Box::new(|row: Row| row.set("status", "uploaded")),
        ),
        ("failed", Box::new(|row: Row| row.set("status", "failed"))),
        (
            "another uploader",
            Box::new(|row: Row| row.set("uploaded_by_user_id", Uuid::from_u128(0xBEEF))),
        ),
    ];
    for (label, edit) in cases {
        let fx = Fx::new().await;
        let id = fx.seed_attachment(1, edit).await;
        let before = fx.snapshot_of_everything().await;
        let error = fx.repo.create_turn(&fx.input(1, &[id])).await.unwrap_err();
        assert!(
            matches!(error, DomainError::InvalidAttachmentIds),
            "{label}: {error:?}"
        );
        assert_rolled_back(&fx, &before).await;
    }
}

#[tokio::test]
async fn an_unknown_deleted_or_foreign_attachment_id_is_invalid() {
    let fx = Fx::new().await;
    let deleted = fx.seed_attachment(1, |row| row).await;
    fx.env.db.soft_delete("attachments", deleted).await;
    let other_chat = fx.env.seed_chat(2).await;
    let foreign = att_id(2);
    fx.env
        .db
        .insert(
            "attachments",
            attachment_row(foreign, USER_TENANT, other_chat).set("uploaded_by_user_id", USER),
        )
        .await
        .unwrap();
    let good = fx.seed_attachment(3, |row| row).await;
    let before = fx.snapshot_of_everything().await;
    for bad in [att_id(99), deleted, foreign] {
        let error = fx
            .repo
            .create_turn(&fx.input(1, &[good, bad]))
            .await
            .unwrap_err();
        assert!(
            matches!(error, DomainError::InvalidAttachmentIds),
            "{error:?}"
        );
        assert_rolled_back(&fx, &before).await;
    }
}

#[tokio::test]
async fn a_failed_limit_recheck_leaves_nothing_behind() {
    let fx = Fx::new().await;
    // The daily total is already at its limit: the re-check fails.
    fx.env
        .seed_bucket(&crate::quota_test_env::BucketSeed::total(
            "daily", 60_000_000, 0,
        ))
        .await;
    let before = fx.snapshot_of_everything().await;
    let error = fx.create(1).await.unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded(crate::domain::error::QuotaSubject::Tokens)
        ),
        "{error:?}"
    );
    assert_rolled_back(&fx, &before).await;
}

// ---- unique violations --------------------------------------------------------------------------

#[tokio::test]
async fn a_second_send_with_the_same_request_id_is_a_unique_violation_and_leaves_nothing() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    // The first turn finished, so only the (chat, request id) keys can fire.
    fx.env
        .db
        .query_i64("UPDATE chat_turns SET state = 'completed', completed_at = '2026-03-10T12:01:00Z' WHERE 1 = 1 RETURNING 1")
        .await;
    let before = fx.snapshot_of_everything().await;
    let error = fx.create(1).await.unwrap_err();
    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_eq!(
        &fx.snapshot_of_everything().await,
        &before,
        "the reserve was rolled back with it"
    );
}

#[tokio::test]
async fn the_messages_index_alone_can_raise_the_unique_violation() {
    let fx = Fx::new().await;
    seed_message(&fx, 7, |r| {
        r.set("request_id", request_id(1)).set("role", "user")
    })
    .await;
    let before = fx.snapshot_of_everything().await;
    let error = fx.create(1).await.unwrap_err();
    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_rolled_back(&fx, &before).await;
}

#[tokio::test]
async fn the_chat_turns_key_alone_can_raise_the_unique_violation() {
    let fx = Fx::new().await;
    seed_turn(&fx, 9, "completed", |row| {
        row.set("request_id", request_id(1))
    })
    .await;
    let before = fx.snapshot_of_everything().await;
    let error = fx.create(1).await.unwrap_err();
    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_rolled_back(&fx, &before).await;
}

#[tokio::test]
async fn the_one_running_turn_index_raises_the_unique_violation_for_another_request() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let before = fx.snapshot_of_everything().await;
    let error = fx.create(2).await.unwrap_err();
    assert!(matches!(error, DomainError::UniqueViolation), "{error:?}");
    assert_eq!(&fx.snapshot_of_everything().await, &before);
}

#[tokio::test]
async fn a_unique_violation_is_never_retried_as_a_conflict() {
    let logs = crate::test_doubles::LogCapture::start();
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let _ = fx.create(2).await.unwrap_err();
    assert!(
        logs.at_containing(tracing::Level::WARN, "retrying")
            .is_empty(),
        "a violation is resolved by the caller, not re-run: {}",
        logs.all_text()
    );
}

#[tokio::test]
async fn concurrent_sends_with_the_same_request_id_create_one_turn() {
    let fx = Fx::on(QuotaEnv::file().await).await;
    let (a, b) = tokio::join!(fx.create(1), fx.create(1));
    let results = [a, b];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Err(DomainError::UniqueViolation)))
    );
    assert_eq!(fx.count("chat_turns").await, 1);
    assert_eq!(fx.count("messages").await, 1);
    assert_eq!(
        fx.env.total("daily").await.unwrap().reserved,
        fx.decision.reserve.reserved_credits_micro,
        "the loser reserved nothing"
    );
}

#[tokio::test]
async fn concurrent_sends_with_different_request_ids_create_one_turn() {
    let fx = Fx::on(QuotaEnv::file().await).await;
    let (a, b) = tokio::join!(fx.create(1), fx.create(2));
    let results = [a, b];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Err(DomainError::UniqueViolation)))
    );
    assert_eq!(fx.count("chat_turns").await, 1);
    assert_eq!(
        fx.env.total("daily").await.unwrap().reserved,
        fx.decision.reserve.reserved_credits_micro
    );
}

// ==== finalization ==============================================================================

async fn finalize(
    fx: &Fx,
    request: &FinalizeRequest,
    writes: &FinalizeWrites,
) -> Result<FinalizeResult, FinalizeError> {
    fx.repo.finalize(request, writes).await
}

#[tokio::test]
async fn a_completed_finalization_writes_the_message_the_turn_and_both_outbox_events() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let request = fx.completed_request(1, Some(usage(120, 34)));
    let result = finalize(&fx, &request, &fx.writes(1, Some("Hello world")))
        .await
        .unwrap();
    assert!(matches!(result, FinalizeResult::Won(_)));
    // The turn.
    let turn = fx.turn(1);
    assert_eq!(fx.col("chat_turns", turn, "state").await, "'completed'");
    assert_eq!(
        fx.col("chat_turns", turn, "assistant_message_id").await,
        blob(assistant_id(1))
    );
    assert_eq!(
        fx.col("chat_turns", turn, "provider_response_id").await,
        "'resp_internal'"
    );
    assert_eq!(fx.col("chat_turns", turn, "error_code").await, "NULL");
    assert_ne!(fx.col("chat_turns", turn, "completed_at").await, "NULL");
    // The assistant message.
    let message = assistant_id(1);
    for (column, expected) in [
        ("tenant_id", blob(USER_TENANT)),
        ("chat_id", blob(fx.chat)),
        ("request_id", blob(request_id(1))),
        ("role", "'assistant'".to_owned()),
        ("content", "'Hello world'".to_owned()),
        ("content_type", "'text'".to_owned()),
        ("token_estimate", "0".to_owned()),
        ("provider_response_id", "'resp_internal'".to_owned()),
        ("request_kind", "'chat'".to_owned()),
        ("features_used", "'[]'".to_owned()),
        ("input_tokens", "120".to_owned()),
        ("output_tokens", "34".to_owned()),
        ("cache_read_input_tokens", "20".to_owned()),
        ("cache_write_input_tokens", "5".to_owned()),
        ("reasoning_tokens", "7".to_owned()),
        ("model", "'p'".to_owned()),
        ("is_compressed", "0".to_owned()),
    ] {
        assert_eq!(
            fx.col("messages", message, column).await,
            expected,
            "messages.{column}"
        );
    }
    assert_eq!(
        fx.at("messages", message, "created_at").await,
        micros_of(now() + Duration::seconds(3))
    );
    // Exactly one usage event and one audit event.
    assert_eq!(fx.env.usage_rows().await.len(), 1);
    assert_eq!(fx.env.outbox_rows(AUDIT_QUEUE).await.len(), 1);
    // The settlement.
    let total = fx.env.total("daily").await.unwrap();
    assert_eq!(total.reserved, 0);
    assert!(total.spent > 0);
}

#[tokio::test]
async fn the_audit_event_is_enqueued_in_the_finalization_with_its_content_fields_empty() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let writes = fx.writes(1, Some("answer"));
    finalize(&fx, &fx.completed_request(1, Some(usage(120, 34))), &writes)
        .await
        .unwrap();
    let rows = fx.env.outbox_rows(AUDIT_QUEUE).await;
    assert_eq!(rows.len(), 1);
    let MiniChatAuditEvent::Turn(event) = decode_audit_event(&rows[0].payload).unwrap() else {
        panic!("a turn audit event expected");
    };
    assert_eq!(event, writes.audit);
    assert_eq!(event.prompt, "");
    assert_eq!(event.response, "");
    assert!(event.attachments.is_empty());
    assert_eq!(
        rows[0].partition,
        i64::from(fx.env.enqueuer.partition(
            crate::infra::outbox::queues::QueueKind::Audit,
            USER_TENANT,
            fx.chat
        )),
        "partitioned by tenant"
    );
}

#[tokio::test]
async fn a_failed_finalization_stores_the_error_columns_and_no_message() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let request = fx.request(
        1,
        TerminalClassification::Failed {
            error_code: "provider_error".to_owned(),
        },
        TerminalColumns {
            state: TerminalState::Failed,
            error_code: Some("provider_error".to_owned()),
            error_detail: Some("provider_error: sanitized".to_owned()),
            assistant_message_id: None,
            provider_response_id: Some("resp_failed".to_owned()),
        },
        Some(usage(10, 0)),
    );
    let mut writes = fx.writes(1, None);
    writes.audit.event_type = TurnAuditEventType::TurnFailed;
    finalize(&fx, &request, &writes).await.unwrap();
    let turn = fx.turn(1);
    assert_eq!(fx.col("chat_turns", turn, "state").await, "'failed'");
    assert_eq!(
        fx.col("chat_turns", turn, "error_code").await,
        "'provider_error'"
    );
    assert_eq!(
        fx.col("chat_turns", turn, "error_detail").await,
        "'provider_error: sanitized'"
    );
    assert_eq!(
        fx.col("chat_turns", turn, "assistant_message_id").await,
        "NULL"
    );
    assert_eq!(fx.count("messages").await, 1, "only the user message");
    assert_eq!(fx.env.usage_rows().await.len(), 1);
    assert_eq!(fx.env.outbox_rows(AUDIT_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn a_cancelled_finalization_keeps_a_partial_message_only_when_given() {
    for (content, expect_message) in [(Some("partial"), true), (None, false)] {
        let fx = Fx::new().await;
        fx.create(1).await.unwrap();
        let request = fx.request(
            1,
            TerminalClassification::Cancelled,
            TerminalColumns {
                state: TerminalState::Cancelled,
                error_code: None,
                error_detail: None,
                assistant_message_id: content.map(|_| assistant_id(1)),
                provider_response_id: None,
            },
            None,
        );
        let mut writes = fx.writes(1, content);
        writes.audit.event_type = TurnAuditEventType::TurnFailed;
        finalize(&fx, &request, &writes).await.unwrap();
        let turn = fx.turn(1);
        assert_eq!(fx.col("chat_turns", turn, "state").await, "'cancelled'");
        assert_eq!(
            fx.count("messages").await,
            if expect_message { 2 } else { 1 }
        );
        assert_eq!(
            fx.col("chat_turns", turn, "assistant_message_id").await,
            if expect_message {
                blob(assistant_id(1))
            } else {
                "NULL".to_owned()
            }
        );
        assert_eq!(fx.env.usage_rows().await.len(), 1);
        assert_eq!(fx.env.outbox_rows(AUDIT_QUEUE).await.len(), 1);
    }
}

#[tokio::test]
async fn a_lost_cas_writes_nothing_at_all() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    // The orphan watchdog got there first.
    fx.env
        .db
        .query_i64("UPDATE chat_turns SET state = 'failed', error_code = 'orphan_timeout', completed_at = '2026-03-10T12:05:00Z' WHERE 1 = 1 RETURNING 1")
        .await;
    let before = fx.snapshot_of_everything().await;
    let rows_before = (
        fx.env.usage_rows().await.len(),
        fx.env.outbox_rows(AUDIT_QUEUE).await.len(),
    );
    let result = finalize(
        &fx,
        &fx.completed_request(1, Some(usage(1, 1))),
        &fx.writes(1, Some("x")),
    )
    .await
    .unwrap();
    assert_eq!(result, FinalizeResult::Lost);
    assert_eq!(
        fx.snapshot_of_everything().await,
        before,
        "no message, no settlement"
    );
    assert_eq!(
        (
            fx.env.usage_rows().await.len(),
            fx.env.outbox_rows(AUDIT_QUEUE).await.len()
        ),
        rows_before,
        "no usage event, no audit event"
    );
}

#[tokio::test]
async fn a_failing_message_insert_is_reported_as_such_and_rolls_everything_back() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let before = fx.snapshot_of_everything().await;
    let mut writes = fx.writes(1, Some("x"));
    // The id of the user message: the INSERT hits the primary key.
    writes.assistant_message.as_mut().unwrap().id = msg_id(1);
    let error = finalize(&fx, &fx.completed_request(1, Some(usage(5, 5))), &writes)
        .await
        .unwrap_err();
    assert!(
        matches!(error, FinalizeError::MessageInsert(_)),
        "{error:?}"
    );
    assert_eq!(
        fx.snapshot_of_everything().await,
        before,
        "the CAS and the settlement rolled back"
    );
    assert_eq!(fx.col("chat_turns", fx.turn(1), "state").await, "'running'");
    assert!(fx.env.usage_rows().await.is_empty());
    assert!(fx.env.outbox_rows(AUDIT_QUEUE).await.is_empty());
}

#[tokio::test]
async fn the_second_run_after_a_failed_insert_commits_the_failed_turn_with_one_event_each() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let mut writes = fx.writes(1, Some("x"));
    writes.assistant_message.as_mut().unwrap().id = msg_id(1);
    let _ = finalize(&fx, &fx.completed_request(1, Some(usage(5, 5))), &writes)
        .await
        .unwrap_err();
    let second = fx.request(
        1,
        TerminalClassification::Failed {
            error_code: "message_persistence_failed".to_owned(),
        },
        TerminalColumns {
            state: TerminalState::Failed,
            error_code: Some("message_persistence_failed".to_owned()),
            error_detail: None,
            assistant_message_id: None,
            provider_response_id: Some("resp_internal".to_owned()),
        },
        Some(usage(5, 5)),
    );
    let mut writes = fx.writes(1, None);
    writes.audit.event_type = TurnAuditEventType::TurnFailed;
    assert!(matches!(
        finalize(&fx, &second, &writes).await.unwrap(),
        FinalizeResult::Won(_)
    ));
    assert_eq!(fx.col("chat_turns", fx.turn(1), "state").await, "'failed'");
    assert_eq!(fx.env.usage_rows().await.len(), 1);
    assert_eq!(fx.env.outbox_rows(AUDIT_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn a_failing_audit_enqueue_rolls_the_whole_finalization_back() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    // An enqueuer whose audit queue is not registered in the running outbox.
    let mismatched = Arc::new(OutboxEnqueuer::new(&OutboxConfig {
        audit_queue_name: "mini-chat.not_registered".to_owned(),
        num_partitions: 4,
        ..OutboxConfig::default()
    }));
    assert!(mismatched.bind(&fx.env.enqueuer.outbox().expect("the pipeline runs")));
    let repo = SeaTurnRepository::new(
        Arc::clone(&fx.env.provider),
        Arc::clone(&fx.env.repo),
        mismatched,
    );
    let before = fx.snapshot_of_everything().await;
    let error = repo
        .finalize(
            &fx.completed_request(1, Some(usage(5, 5))),
            &fx.writes(1, Some("x")),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, FinalizeError::Other(_)), "{error:?}");
    assert_eq!(fx.snapshot_of_everything().await, before);
    assert_eq!(fx.col("chat_turns", fx.turn(1), "state").await, "'running'");
    assert!(
        fx.env.usage_rows().await.is_empty(),
        "the usage event rolled back too"
    );
}

#[tokio::test]
async fn the_finalization_of_a_turn_of_another_tenant_is_lost() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let mut request = fx.completed_request(1, Some(usage(1, 1)));
    request.variant = FinalizeVariant::Stream {
        tenant_id: OTHER_TENANT,
        period_keys: fx.decision.period_keys,
        selected_model: "p".to_owned(),
    };
    assert_eq!(
        finalize(&fx, &request, &fx.writes(1, Some("x")))
            .await
            .unwrap(),
        FinalizeResult::Lost
    );
    assert_eq!(fx.col("chat_turns", fx.turn(1), "state").await, "'running'");
}

#[tokio::test]
async fn the_assistant_message_sorts_after_its_question() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let mut writes = fx.writes(1, Some("answer"));
    // The clock did not move: the answer is a microsecond after the question.
    writes.assistant_message.as_mut().unwrap().created_at = now() + Duration::microseconds(1);
    finalize(&fx, &fx.completed_request(1, Some(usage(5, 5))), &writes)
        .await
        .unwrap();
    let order = fx
        .env
        .db
        .query_string(
            "SELECT group_concat(role, ',') FROM (SELECT role FROM messages ORDER BY created_at, id)",
        )
        .await;
    assert_eq!(order, "user,assistant");
}

// ==== liveness and progress =====================================================================

#[tokio::test]
async fn the_turn_state_is_read_by_tenant_chat_and_turn() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let turn = fx.turn(1);
    assert_eq!(
        fx.repo
            .turn_state(USER_TENANT, fx.chat, turn)
            .await
            .unwrap(),
        Some(TurnState::Running)
    );
    fx.env
        .db
        .query_i64("UPDATE chat_turns SET state = 'failed' WHERE 1 = 1 RETURNING 1")
        .await;
    assert_eq!(
        fx.repo
            .turn_state(USER_TENANT, fx.chat, turn)
            .await
            .unwrap(),
        Some(TurnState::Failed)
    );
}

#[tokio::test]
async fn the_liveness_query_is_tenant_scoped_and_chat_bound() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let turn = fx.turn(1);
    let other_chat = fx.env.seed_chat(2).await;
    assert_eq!(
        fx.repo
            .turn_state(OTHER_TENANT, fx.chat, turn)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        fx.repo
            .turn_state(USER_TENANT, other_chat, turn)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        fx.repo
            .turn_state(USER_TENANT, fx.chat, Uuid::from_u128(0xDEAD))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn progress_updates_only_a_running_turn() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let turn = fx.turn(1);
    let later = now() + Duration::seconds(40);
    fx.repo
        .record_progress(
            USER_TENANT,
            fx.chat,
            turn,
            &ProgressUpdate {
                at: Some(later),
                web_search_done: Some(2),
                code_interpreter_done: None,
                file_search_done: Some(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fx.col("chat_turns", turn, "web_search_completed_count")
            .await,
        "2"
    );
    assert_eq!(
        fx.col("chat_turns", turn, "code_interpreter_completed_count")
            .await,
        "0"
    );
    assert_eq!(
        fx.col("chat_turns", turn, "file_search_completed_count")
            .await,
        "1"
    );
    assert_eq!(
        fx.at("chat_turns", turn, "last_progress_at").await,
        micros_of(later)
    );

    // A terminal turn is never touched.
    fx.env
        .db
        .query_i64("UPDATE chat_turns SET state = 'completed' WHERE 1 = 1 RETURNING 1")
        .await;
    fx.repo
        .record_progress(
            USER_TENANT,
            fx.chat,
            turn,
            &ProgressUpdate {
                at: Some(later + Duration::seconds(60)),
                web_search_done: Some(9),
                ..ProgressUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fx.col("chat_turns", turn, "web_search_completed_count")
            .await,
        "2"
    );
    assert_eq!(
        fx.at("chat_turns", turn, "last_progress_at").await,
        micros_of(later)
    );
}

#[tokio::test]
async fn a_progress_write_is_bound_to_the_tenant_and_the_chat() {
    let fx = Fx::new().await;
    fx.create(1).await.unwrap();
    let turn = fx.turn(1);
    let update = ProgressUpdate {
        at: Some(now() + Duration::seconds(40)),
        web_search_done: Some(5),
        ..ProgressUpdate::default()
    };
    fx.repo
        .record_progress(OTHER_TENANT, fx.chat, turn, &update)
        .await
        .unwrap();
    fx.repo
        .record_progress(USER_TENANT, fx.env.seed_chat(2).await, turn, &update)
        .await
        .unwrap();
    assert_eq!(
        fx.col("chat_turns", turn, "web_search_completed_count")
            .await,
        "0"
    );
}
