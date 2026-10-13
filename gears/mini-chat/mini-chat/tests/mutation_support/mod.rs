//! S5 (turn mutations and the orphan watchdog) harness on top of the S1 fakes, the S2 chat harness,
//! the S3 stream pieces and the S4 quota pieces.
//!
//! Kept apart from `common`, `chat_support`, `stream_support` and `quota_support` so the S1 - S4
//! test binaries (which never declare `mod mutation_support;`) compile independently of the S5
//! production items. Everything here relies only on the items listed in `S5-test-interface.md`.
//! Test files declare `mod common; mod chat_support; mod stream_support; mod quota_support;
//! mod mutation_support;`.
//!
//! * [`MutationHarness`] wires the real `StreamService` (with the usage and audit outbox adapters),
//!   `TurnsService`, `TurnMutationService` (retry / edit / delete) and `OrphanWatchdog` over the S2
//!   chat harness database, together with a second running outbox that serves the usage queue and
//!   the audit queue (the same arrangement as the S4 `QuotaHarness`, which it does not extend: the
//!   S4 harness keeps its stream service private).
//! * [`PairSeed`] / [`TurnRowSeed`] insert completed, failed, cancelled and running turns directly,
//!   with every column the mutation rules and the watchdog read.
//! * [`Snapshot`] fingerprints everything a rejected mutation must not change.

#![allow(
    dead_code,
    unused_imports,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::integer_division,
    clippy::struct_excessive_bools
)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body as AxumBody;
use axum::http::{Request, header};
use mini_chat::api::rest::{
    register_quota_routes, register_stream_routes, register_turn_mutation_routes,
};
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::quota_status::QuotaStatusService;
use mini_chat::domain::streaming::StreamService;
use mini_chat::domain::turn_mutations::TurnMutationService;
use mini_chat::domain::turns::TurnsService;
use mini_chat::domain::watchdog::OrphanWatchdog;
use mini_chat::infra::db::entity::{
    chat_turns, message_attachments, messages, quota_usage, thread_summaries,
};
use mini_chat::infra::db::timestamp::{to_db, to_db_opt};
use mini_chat::infra::llm::LlmProviders;
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat::infra::outbox::{AuditOutbox, AuditOutboxHandler, UsageOutbox, UsageOutboxHandler};
use mini_chat_sdk::MiniChatAuditPluginClientV1;
use oagw_sdk::api::ServiceGatewayClientV1;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{Date as DbDate, DateTimeUtc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_db::outbox::{OutboxHandle, OutboxProfile, Partitions};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

pub use crate::chat_support::{
    AttachmentSeed, ChatHarness, ChatHarnessOptions, ChatSeed, MessageSeed, USER_A2,
    assert_field_violation, assert_not_found, assert_precondition, assert_violation_reason,
    from_db, page_items, parse_ts, ts, ts_micros, uid,
};
pub use crate::common::{
    FakePdp, PdpMode, Resp, TENANT_A, TENANT_B, USER_A, USER_B, assert_problem, ctx, ctx_a,
};
pub use crate::quota_support::{
    LimitsSpec, ObservingOagw, PREMIUM, PREMIUM_IN, PREMIUM_OUT, QuotaPolicy, RecordingAudit,
    STANDARD, STANDARD_IN, STANDARD_OUT, answer_with_usage, assert_exhausted, audit_gateway_for,
    dedupe_key, find_model, period_start_today, quota_models, ref_credits, ref_estimate,
    ref_reserve, standard_two,
};
pub use crate::stream_support::{
    FakeOagw, LiveSse, Opts, Piece, ProviderReply, RawStream, Sent, assert_400_reason,
    assert_conflict, contains_provider_identifier, ev, eventually, failed_stream,
    file_search_pieces, hex, merge, model_entries, responses_stream, second_user, stream_config,
    stream_model_json, tenant_b_user, web_search_pieces,
};

/// `streaming.max_output_tokens` default (caps nothing for the 4096-token catalog models).
pub const STREAMING_MAX_OUTPUT: u32 = 32_768;

// ── operations under test ────────────────────────────────────────────────────

/// The three turn mutations; the rejection matrices run every case over all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    Retry,
    /// `PATCH /v1/chats/{id}/turns/{request_id}` with `{"content": EDIT_TEXT}`.
    Edit,
    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    Delete,
}

pub const ALL_OPS: [Op; 3] = [Op::Retry, Op::Edit, Op::Delete];

/// Replacement text the matrix tests send for [`Op::Edit`].
pub const EDIT_TEXT: &str = "replacement text";

impl Op {
    /// PDP action name of the operation (DESIGN 3.8).
    pub const fn action(self) -> &'static str {
        match self {
            Self::Retry => "retry_turn",
            Self::Edit => "edit_turn",
            Self::Delete => "delete_turn",
        }
    }
}

// ── options ──────────────────────────────────────────────────────────────────

pub struct MutationOptions {
    pub models: Vec<Value>,
    pub pdp: Arc<FakePdp>,
    /// Merged into the gear configuration (default: `orphan_watchdog.timeout_secs = 90`,
    /// `scan_interval_secs = 1`).
    pub config: Value,
    pub limits: LimitsSpec,
    pub policy_version: u64,
    /// `false`: the audit queue is not registered under its configured name, so every audit
    /// enqueue fails (`QueueNotRegistered`).
    pub register_audit_queue: bool,
}

impl Default for MutationOptions {
    fn default() -> Self {
        Self {
            models: quota_models(),
            pdp: FakePdp::allow(),
            config: json!({}),
            limits: LimitsSpec::generous(),
            policy_version: 1,
            register_audit_queue: true,
        }
    }
}

// ── seeds ────────────────────────────────────────────────────────────────────

/// Reserve columns of a seeded turn.
#[derive(Clone, Copy, Debug)]
pub struct SeedReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens: i32,
    pub reserved_credits_micro: i64,
    pub policy_version: i64,
    pub floor: i32,
}

/// A `chat_turns` row to insert directly (every column the rules and the watchdog read).
#[derive(Clone, Debug)]
pub struct TurnRowSeed {
    pub id: Uuid,
    pub tenant: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub state: String,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub requester: Option<Uuid>,
    pub effective_model: Option<String>,
    pub reserve: Option<SeedReserve>,
    pub deleted_at: Option<OffsetDateTime>,
    pub replaced_by: Option<Uuid>,
    pub started_at: OffsetDateTime,
    pub last_progress_at: Option<OffsetDateTime>,
    pub web_search_enabled: bool,
    pub web_search_done: i32,
    pub code_interpreter_done: i32,
    pub file_search_done: i32,
}

impl TurnRowSeed {
    /// A `running` turn of `USER_A` started at `ts(10)` with fresh progress and no reserve.
    pub fn running(chat_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant: TENANT_A,
            chat_id,
            request_id: Uuid::new_v4(),
            state: "running".to_owned(),
            error_code: None,
            assistant_message_id: None,
            requester: Some(USER_A),
            effective_model: Some(PREMIUM.to_owned()),
            reserve: None,
            deleted_at: None,
            replaced_by: None,
            started_at: ts(10),
            last_progress_at: Some(ts(10)),
            web_search_enabled: false,
            web_search_done: 0,
            code_interpreter_done: 0,
            file_search_done: 0,
        }
    }

    /// A terminal turn (`completed`, `failed` or `cancelled`).
    pub fn terminal(chat_id: Uuid, state: &str) -> Self {
        Self {
            state: state.to_owned(),
            ..Self::running(chat_id)
        }
    }

    pub fn request(mut self, request_id: Uuid) -> Self {
        self.request_id = request_id;
        self
    }

    /// The tenant of the row (and its requester) when the chat is not tenant A's.
    pub fn of(mut self, tenant: Uuid, user: Uuid) -> Self {
        self.tenant = tenant;
        self.requester = Some(user);
        self
    }

    /// `started_at` (and, unless set later, the progress timestamp) at `when`.
    pub fn started(mut self, when: OffsetDateTime) -> Self {
        self.started_at = when;
        self.last_progress_at = Some(when);
        self
    }

    pub fn progress(mut self, when: Option<OffsetDateTime>) -> Self {
        self.last_progress_at = when;
        self
    }

    /// A retry / edit turn left `running` before its preflight fields were written: no
    /// `effective_model`, no reserve columns (DESIGN 3.7 "Preflight columns on retry/edit").
    pub fn bare(mut self) -> Self {
        self.effective_model = None;
        self.reserve = None;
        self
    }

    pub fn effective(mut self, model: &str) -> Self {
        self.effective_model = Some(model.to_owned());
        self
    }

    pub fn reserve(mut self, reserve: SeedReserve) -> Self {
        self.reserve = Some(reserve);
        self
    }

    pub fn requester(mut self, user: Option<Uuid>) -> Self {
        self.requester = user;
        self
    }

    pub fn deleted(mut self, when: OffsetDateTime) -> Self {
        self.deleted_at = Some(when);
        self
    }

    pub fn web_search(mut self, enabled: bool) -> Self {
        self.web_search_enabled = enabled;
        self
    }

    /// Completed tool calls counted on the turn.
    pub fn tools(mut self, web_search: i32, code_interpreter: i32, file_search: i32) -> Self {
        self.web_search_done = web_search;
        self.code_interpreter_done = code_interpreter;
        self.file_search_done = file_search;
        self
    }
}

/// A turn as the API leaves it: the user message, the assistant message (when there is one) and
/// the `chat_turns` row.
#[derive(Clone, Debug)]
pub struct PairSeed {
    pub state: String,
    pub error_code: Option<String>,
    pub question: String,
    pub answer: Option<String>,
    /// Seconds after 2025-01-01: `started_at` and the user message instant (the answer is one
    /// second later).
    pub at: i64,
    pub request_id: Uuid,
    pub requester: Uuid,
    pub web_search: bool,
    pub tokens: (i64, i64),
    pub deleted: bool,
    pub model: String,
    /// Fixed id of the user message (a random one otherwise).
    pub user_id: Option<Uuid>,
}

impl PairSeed {
    pub fn completed(question: &str, answer: &str) -> Self {
        Self {
            state: "completed".to_owned(),
            error_code: None,
            question: question.to_owned(),
            answer: Some(answer.to_owned()),
            at: 10,
            request_id: Uuid::new_v4(),
            requester: USER_A,
            web_search: false,
            tokens: (0, 0),
            deleted: false,
            model: PREMIUM.to_owned(),
            user_id: None,
        }
    }

    /// A `failed` turn (`provider_error`) with no assistant message.
    pub fn failed(question: &str) -> Self {
        Self {
            state: "failed".to_owned(),
            error_code: Some("provider_error".to_owned()),
            answer: None,
            ..Self::completed(question, "")
        }
    }

    /// A `cancelled` turn; `partial` is the stored partial answer, when there is one.
    pub fn cancelled(question: &str, partial: Option<&str>) -> Self {
        Self {
            state: "cancelled".to_owned(),
            answer: partial.map(str::to_owned),
            ..Self::completed(question, "")
        }
    }

    pub fn at(mut self, secs: i64) -> Self {
        self.at = secs;
        self
    }

    pub fn request(mut self, request_id: Uuid) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn requester(mut self, user: Uuid) -> Self {
        self.requester = user;
        self
    }

    pub fn web_search(mut self) -> Self {
        self.web_search = true;
        self
    }

    /// Provider token counts stored on the assistant message.
    pub fn tokens(mut self, input: i64, output: i64) -> Self {
        self.tokens = (input, output);
        self
    }

    /// An already soft-deleted turn (replaced by a retry / edit or deleted).
    pub fn deleted(mut self) -> Self {
        self.deleted = true;
        self
    }

    pub fn model(mut self, model: &str) -> Self {
        model.clone_into(&mut self.model);
        self
    }

    /// Fix the id of the user message (the summary frontier tie-break tests need it).
    pub fn user_id(mut self, id: Uuid) -> Self {
        self.user_id = Some(id);
        self
    }
}

/// Ids of a seeded [`PairSeed`].
#[derive(Clone, Copy, Debug)]
pub struct Pair {
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_message: Uuid,
    pub assistant_message: Option<Uuid>,
    pub started_at: OffsetDateTime,
}

// ── snapshots ────────────────────────────────────────────────────────────────

/// `(request_id, state, error_code, deleted_at, replaced_by_request_id)` of one turn.
pub type TurnFingerprint = (
    Uuid,
    String,
    Option<String>,
    Option<DateTimeUtc>,
    Option<Uuid>,
);

/// Everything a rejected or failed-before-commit mutation must leave unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// One entry per turn, sorted by request id.
    pub turns: Vec<TurnFingerprint>,
    /// `(id, deleted_at, is_compressed)` sorted by id.
    pub messages: Vec<(Uuid, Option<DateTimeUtc>, bool)>,
    /// `(message_id, attachment_id)` sorted.
    pub links: Vec<(Uuid, Uuid)>,
    /// Chat ids that have a `thread_summaries` row, sorted.
    pub summaries: Vec<Uuid>,
    /// `(bucket, period, spent, reserved, calls)` sorted.
    pub usage: Vec<(String, String, i64, i64, i32)>,
    /// `chats.updated_at` of every chat, sorted by chat id.
    pub chats: Vec<(Uuid, DateTimeUtc)>,
}

// ── the harness ──────────────────────────────────────────────────────────────

pub struct MutationHarness {
    /// The S2 harness: database, chat routes, seeds.
    pub chat: ChatHarness,
    /// Chat, stream, turn-status, quota and turn-mutation routes.
    pub router: Router,
    /// The scripted provider (answers, recorded requests, pauses).
    pub oagw: Arc<FakeOagw>,
    /// The same provider seen through the database observer (state at each provider call).
    pub observed: Arc<ObservingOagw>,
    pub policy: Arc<QuotaPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub config: Arc<MiniChatConfig>,
    pub pdp: Arc<FakePdp>,
    /// The orphan watchdog over the same database and outbox ports (driven by `sweep`).
    pub watchdog: Arc<OrphanWatchdog>,
    _outbox: OutboxHandle,
}

impl MutationHarness {
    pub async fn new() -> Self {
        Self::with(MutationOptions::default()).await
    }

    pub async fn with_config(config: Value) -> Self {
        Self::with(MutationOptions {
            config,
            ..Default::default()
        })
        .await
    }

    pub async fn with_limits(limits: LimitsSpec) -> Self {
        Self::with(MutationOptions {
            limits,
            ..Default::default()
        })
        .await
    }

    pub async fn with_models(models: Vec<Value>) -> Self {
        Self::with(MutationOptions {
            models,
            ..Default::default()
        })
        .await
    }

    pub async fn with(opts: MutationOptions) -> Self {
        let chat = crate::chat_support::chat_harness_with(ChatHarnessOptions {
            models: model_entries(&opts.models),
            pdp: opts.pdp.clone(),
            ..Default::default()
        })
        .await;
        let oagw = FakeOagw::new();
        let policy = QuotaPolicy::new(&opts.models, opts.policy_version, opts.limits);
        let audit = Arc::new(RecordingAudit::default());
        let mut cfg = json!({"orphan_watchdog": {"timeout_secs": 90, "scan_interval_secs": 1}});
        merge(&mut cfg, &opts.config);
        let config = stream_config(&cfg);

        let gateway = Arc::new(ModelPolicyGateway::from_client(policy.clone()));
        let audit_plugin: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        let audit_gateway = audit_gateway_for(&config.vendor, Some(audit_plugin));
        let partitions = u16::try_from(config.outbox.num_partitions).expect("partitions");
        let audit_queue = if opts.register_audit_queue {
            config.outbox.audit_queue_name.clone()
        } else {
            "mini-chat.unrelated_audit".to_owned()
        };
        // The S2 harness already runs an outbox on this database; SQLite answers `database is
        // locked` (no busy wait) to a registration that races its workers, so starting ours is
        // retried.
        let mut attempts = 0;
        let outbox = loop {
            let started = toolkit_db::outbox::Outbox::builder(chat.db.db())
                .profile(OutboxProfile::low_latency())
                .queue(&config.outbox.queue_name, Partitions::of(partitions))
                .leased(UsageOutboxHandler::new(gateway.clone()))
                .queue(&audit_queue, Partitions::of(partitions))
                .leased(AuditOutboxHandler::new(audit_gateway.clone()))
                .start()
                .await;
            match started {
                Ok(outbox) => break outbox,
                Err(error) if attempts < 40 => {
                    attempts += 1;
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("usage / audit outbox starts: {error:?}"),
            }
        };
        let usage_port = Arc::new(UsageOutbox::new(
            outbox.outbox().clone(),
            config.outbox.queue_name.clone(),
            partitions,
        ));
        let audit_port = Arc::new(AuditOutbox::new(
            outbox.outbox().clone(),
            config.outbox.audit_queue_name.clone(),
            partitions,
        ));

        let observed = ObservingOagw::new(oagw.clone(), chat.db.clone());
        let oagw_client: Arc<dyn ServiceGatewayClientV1> = observed.clone();
        let llm = Arc::new(LlmProviders::new(oagw_client, &config));
        let enforcer = opts.pdp.enforcer();
        let stream = Arc::new(
            StreamService::new(
                chat.db.clone(),
                gateway.clone(),
                llm,
                config.clone(),
                enforcer.clone(),
            )
            .with_outbox(usage_port.clone(), audit_port.clone()),
        );
        let turns = Arc::new(TurnsService::new(chat.db.clone(), enforcer.clone()));
        let quota = Arc::new(QuotaStatusService::new(
            chat.db.clone(),
            gateway.clone(),
            config.clone(),
            enforcer.clone(),
        ));
        let mutations = Arc::new(
            TurnMutationService::new(chat.db.clone(), stream.clone(), enforcer)
                .with_audit(audit_port.clone()),
        );
        let watchdog = Arc::new(
            OrphanWatchdog::new(chat.db.clone(), gateway, config.clone())
                .with_outbox(usage_port, audit_port),
        );
        let router = register_stream_routes(
            chat.router.clone(),
            chat.openapi.as_ref(),
            stream,
            turns,
            &chat.prefix,
        );
        let router = register_quota_routes(router, chat.openapi.as_ref(), quota, &chat.prefix);
        let router =
            register_turn_mutation_routes(router, chat.openapi.as_ref(), mutations, &chat.prefix);
        Self {
            chat,
            router,
            oagw,
            observed,
            policy,
            audit,
            config,
            pdp: opts.pdp,
            watchdog,
            _outbox: outbox,
        }
    }

    /// `orphan_watchdog.timeout_secs` as a duration.
    pub fn timeout(&self) -> time::Duration {
        time::Duration::seconds(i64::try_from(self.config.orphan_watchdog.timeout_secs).unwrap())
    }

    // ── URIs ─────────────────────────────────────────────────────────────────

    pub fn stream_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/messages:stream", self.chat.prefix)
    }

    pub fn turn_uri(&self, chat: Uuid, request_id: Uuid) -> String {
        format!("{}/v1/chats/{chat}/turns/{request_id}", self.chat.prefix)
    }

    pub fn retry_uri(&self, chat: Uuid, request_id: Uuid) -> String {
        format!("{}/retry", self.turn_uri(chat, request_id))
    }

    // ── chats ────────────────────────────────────────────────────────────────

    /// A chat of `USER_A` on `gpt-premium`.
    pub async fn seed_chat(&self) -> Uuid {
        self.seed_chat_on(PREMIUM).await
    }

    pub async fn seed_chat_on(&self, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_model(model).at(0))
            .await
    }

    /// A chat of another user (`user`, `tenant`) on `model`.
    pub async fn seed_chat_of(&self, user: Uuid, tenant: Uuid, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::new(tenant, user).with_model(model).at(0))
            .await
    }

    // ── turn seeds ───────────────────────────────────────────────────────────

    /// Insert a `chat_turns` row directly; returns its request id.
    pub async fn insert_turn(&self, seed: TurnRowSeed) -> Uuid {
        let conn = self.chat.db.conn().expect("db connection");
        let terminal = seed.state != "running";
        let updated = seed.last_progress_at.unwrap_or(seed.started_at) + time::Duration::seconds(1);
        let reserve = seed.reserve;
        secure_insert::<chat_turns::Entity>(
            chat_turns::ActiveModel {
                id: Set(seed.id),
                tenant_id: Set(seed.tenant),
                chat_id: Set(seed.chat_id),
                request_id: Set(seed.request_id),
                requester_type: Set("user".to_owned()),
                requester_user_id: Set(seed.requester),
                state: Set(seed.state),
                provider_name: Set(None),
                provider_response_id: Set(None),
                assistant_message_id: Set(seed.assistant_message_id),
                error_code: Set(seed.error_code),
                reserve_tokens: Set(reserve.map(|r| r.reserve_tokens)),
                max_output_tokens_applied: Set(reserve.map(|r| r.max_output_tokens)),
                reserved_credits_micro: Set(reserve.map(|r| r.reserved_credits_micro)),
                policy_version_applied: Set(reserve.map(|r| r.policy_version)),
                effective_model: Set(seed.effective_model),
                minimal_generation_floor_applied: Set(reserve.map(|r| r.floor)),
                error_detail: Set(None),
                deleted_at: Set(to_db_opt(seed.deleted_at)),
                replaced_by_request_id: Set(seed.replaced_by),
                started_at: Set(to_db(seed.started_at)),
                last_progress_at: Set(to_db_opt(seed.last_progress_at)),
                web_search_enabled: Set(seed.web_search_enabled),
                web_search_completed_count: Set(seed.web_search_done),
                code_interpreter_completed_count: Set(seed.code_interpreter_done),
                file_search_completed_count: Set(seed.file_search_done),
                completed_at: Set(terminal.then(|| to_db(updated))),
                updated_at: Set(Some(to_db(updated))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed turn");
        seed.request_id
    }

    /// Seed one turn the way the API leaves it: user message, assistant message (when `answer`
    /// is set) and the `chat_turns` row, all at `seed.at`.
    pub async fn seed_pair(&self, chat: Uuid, seed: PairSeed) -> Pair {
        let started_at = ts(seed.at);
        let mut user = MessageSeed::user(chat)
            .request(Some(seed.request_id))
            .content(&seed.question)
            .at(seed.at);
        if let Some(id) = seed.user_id {
            user.id = id;
        }
        if seed.deleted {
            user = user.deleted();
        }
        let user_message = self.chat.seed_message(user).await;
        let assistant_message = match &seed.answer {
            Some(answer) => {
                let mut assistant = MessageSeed::assistant(chat)
                    .request(Some(seed.request_id))
                    .content(answer)
                    .model(&seed.model)
                    .tokens(seed.tokens.0, seed.tokens.1)
                    .at(seed.at + 1);
                if seed.deleted {
                    assistant = assistant.deleted();
                }
                Some(self.chat.seed_message(assistant).await)
            }
            None => None,
        };
        let mut row = TurnRowSeed::terminal(chat, &seed.state)
            .request(seed.request_id)
            .started(started_at)
            .effective(&seed.model)
            .requester(Some(seed.requester))
            .web_search(seed.web_search);
        row.error_code = seed.error_code.clone();
        row.assistant_message_id = assistant_message;
        if seed.deleted {
            row.deleted_at = Some(started_at + time::Duration::seconds(5));
        }
        let turn_id = row.id;
        self.insert_turn(row).await;
        Pair {
            request_id: seed.request_id,
            turn_id,
            user_message,
            assistant_message,
            started_at,
        }
    }

    /// `n` completed turns `question i` / `answer i` (zero token counts) at 10 s, 20 s, ...
    pub async fn seed_history(&self, chat: Uuid, n: usize) -> Vec<Pair> {
        let mut pairs = Vec::new();
        for i in 1..=n {
            pairs.push(
                self.seed_pair(
                    chat,
                    PairSeed::completed(&format!("question {i}"), &format!("answer {i}"))
                        .at(i64::try_from(i).unwrap() * 10),
                )
                .await,
            );
        }
        pairs
    }

    /// A ready document + the `chat_vector_stores` row that makes `file_search` available.
    pub async fn seed_document_with_store(&self, chat: Uuid) -> Uuid {
        let attachment = self
            .chat
            .seed_attachment(AttachmentSeed::document(chat, "doc.pdf"))
            .await;
        let conn = self.chat.db.conn().expect("db connection");
        secure_insert::<mini_chat::infra::db::entity::chat_vector_stores::Entity>(
            mini_chat::infra::db::entity::chat_vector_stores::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(TENANT_A),
                chat_id: Set(chat),
                vector_store_id: Set(Some("vs_mutation_test_store".to_owned())),
                provider: Set("openai".to_owned()),
                file_count: Set(1),
                created_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed vector store");
        attachment
    }

    /// Insert a `thread_summaries` row whose frontier is `(frontier_at, frontier_message)`.
    pub async fn seed_summary(
        &self,
        chat: Uuid,
        frontier_at: OffsetDateTime,
        frontier_message: Uuid,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let conn = self.chat.db.conn().expect("db connection");
        secure_insert::<thread_summaries::Entity>(
            thread_summaries::ActiveModel {
                id: Set(id),
                tenant_id: Set(TENANT_A),
                chat_id: Set(chat),
                summary_text: Set(Some("a summary of the earlier conversation".to_owned())),
                summarized_up_to_created_at: Set(to_db(frontier_at)),
                summarized_up_to_message_id: Set(frontier_message),
                token_estimate: Set(Some(12)),
                created_at: Set(to_db(ts(0))),
                updated_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed summary");
        id
    }

    /// Set `is_compressed = true` on `ids` (what a summary does to the messages it covers).
    pub async fn mark_compressed(&self, ids: &[Uuid]) {
        let conn = self.chat.db.conn().expect("db connection");
        messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(true))
            .filter(messages::Column::Id.is_in(ids.iter().copied()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("mark compressed");
    }

    // ── sending ──────────────────────────────────────────────────────────────

    /// `POST messages:stream` as `ctx`; the body is not read.
    pub async fn open(&self, chat: Uuid, ctx: &SecurityContext, body: &Value) -> RawStream {
        self.raw(
            "POST",
            &self.stream_uri(chat),
            Some(ctx),
            Some("application/json"),
            &body.to_string(),
        )
        .await
    }

    /// Any request with an optional security context, content type and raw body.
    pub async fn raw(
        &self,
        method: &str,
        uri: &str,
        ctx: Option<&SecurityContext>,
        content_type: Option<&str>,
        body: &str,
    ) -> RawStream {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let mut req = builder
            .body(AxumBody::from(body.to_owned()))
            .expect("request");
        if let Some(ctx) = ctx {
            req.extensions_mut().insert(ctx.clone());
        }
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        RawStream::from_response(resp)
    }

    /// Send as `USER_A`, read the whole SSE answer.
    pub async fn send(&self, chat: Uuid, body: &Value) -> Sent {
        self.open(chat, &ctx_a(), body).await.collect().await
    }

    pub async fn say(&self, chat: Uuid, text: &str) -> Sent {
        self.send(chat, &json!({"content": text})).await
    }

    pub async fn say_with(&self, chat: Uuid, text: &str, request_id: Uuid) -> Sent {
        self.send(chat, &json!({"content": text, "request_id": request_id}))
            .await
    }

    /// A message with `web_search.enabled = true`.
    pub async fn say_web(&self, chat: Uuid, text: &str) -> Sent {
        self.send(
            chat,
            &json!({"content": text, "web_search": {"enabled": true}}),
        )
        .await
    }

    /// Send as `USER_A` and expect a JSON rejection (no SSE stream).
    pub async fn reject_send(&self, chat: Uuid, body: Value) -> Resp {
        let raw = self.open(chat, &ctx_a(), &body).await;
        assert!(
            !raw.is_sse(),
            "expected a JSON rejection, got an SSE stream ({})",
            raw.content_type()
        );
        raw.into_resp().await
    }

    /// Start a turn that stays `running`: the provider pauses after its first delta. Returns the
    /// live stream (positioned after that delta) and the request id.
    pub async fn hold_running(&self, chat: Uuid, text: &str) -> (LiveSse, Uuid) {
        let rid = Uuid::new_v4();
        self.oagw.push(ProviderReply::text_with(
            &["held", " back"],
            &Opts::default().pause_after(1),
        ));
        let mut live = self
            .open(chat, &ctx_a(), &json!({"content": text, "request_id": rid}))
            .await
            .live();
        live.expect("stream_started").await;
        live.next_non_ping().await.expect("first delta");
        self.oagw.wait_paused(1).await;
        (live, rid)
    }

    /// Start a turn whose provider stalls before the first delta (what a hung provider looks
    /// like): `stream_started` was read, nothing else arrives until `oagw.release()`.
    pub async fn stall(&self, chat: Uuid, text: &str) -> (LiveSse, Uuid) {
        let rid = Uuid::new_v4();
        self.oagw.push(ProviderReply::text_with(
            &["late answer"],
            &Opts::default().pause_after(0),
        ));
        let mut live = self
            .open(chat, &ctx_a(), &json!({"content": text, "request_id": rid}))
            .await
            .live();
        live.expect("stream_started").await;
        self.oagw.wait_paused(1).await;
        (live, rid)
    }

    // ── the mutations ────────────────────────────────────────────────────────

    pub async fn retry_as(&self, chat: Uuid, rid: Uuid, ctx: &SecurityContext) -> RawStream {
        self.raw("POST", &self.retry_uri(chat, rid), Some(ctx), None, "")
            .await
    }

    pub async fn edit_as(
        &self,
        chat: Uuid,
        rid: Uuid,
        ctx: &SecurityContext,
        content: &str,
    ) -> RawStream {
        self.raw(
            "PATCH",
            &self.turn_uri(chat, rid),
            Some(ctx),
            Some("application/json"),
            &json!({"content": content}).to_string(),
        )
        .await
    }

    pub async fn delete_as(&self, chat: Uuid, rid: Uuid, ctx: &SecurityContext) -> Resp {
        self.raw("DELETE", &self.turn_uri(chat, rid), Some(ctx), None, "")
            .await
            .into_resp()
            .await
    }

    /// Retry as `USER_A`; the body is not read.
    pub async fn retry(&self, chat: Uuid, rid: Uuid) -> RawStream {
        self.retry_as(chat, rid, &ctx_a()).await
    }

    pub async fn edit(&self, chat: Uuid, rid: Uuid, content: &str) -> RawStream {
        self.edit_as(chat, rid, &ctx_a(), content).await
    }

    pub async fn delete(&self, chat: Uuid, rid: Uuid) -> Resp {
        self.delete_as(chat, rid, &ctx_a()).await
    }

    /// Retry as `USER_A` and read the whole SSE answer.
    pub async fn retried(&self, chat: Uuid, rid: Uuid) -> Sent {
        self.retry(chat, rid).await.collect().await
    }

    pub async fn edited(&self, chat: Uuid, rid: Uuid, content: &str) -> Sent {
        self.edit(chat, rid, content).await.collect().await
    }

    /// Run `op` as `ctx` and expect a JSON answer (never an SSE stream).
    pub async fn reject_op(&self, op: Op, chat: Uuid, rid: Uuid, ctx: &SecurityContext) -> Resp {
        let raw = match op {
            Op::Retry => self.retry_as(chat, rid, ctx).await,
            Op::Edit => self.edit_as(chat, rid, ctx, EDIT_TEXT).await,
            Op::Delete => return self.delete_as(chat, rid, ctx).await,
        };
        assert!(
            !raw.is_sse(),
            "{op:?}: expected a JSON rejection, got an SSE stream"
        );
        raw.into_resp().await
    }

    /// `reject_op` as `USER_A`.
    pub async fn reject(&self, op: Op, chat: Uuid, rid: Uuid) -> Resp {
        self.reject_op(op, chat, rid, &ctx_a()).await
    }

    // ── the watchdog ─────────────────────────────────────────────────────────

    /// One watchdog scan at the application time `now`; returns the number of turns it finalized.
    pub async fn sweep(&self, now: OffsetDateTime) -> usize {
        self.watchdog.sweep_at(now).await.expect("watchdog sweep")
    }

    /// Rewrite the liveness timestamps of a running turn (what time passing does).
    pub async fn set_liveness(
        &self,
        request_id: Uuid,
        started_at: OffsetDateTime,
        last_progress_at: Option<OffsetDateTime>,
    ) {
        let conn = self.chat.db.conn().expect("db connection");
        let updated = chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::StartedAt,
                Expr::value(to_db(started_at)),
            )
            .col_expr(
                chat_turns::Column::LastProgressAt,
                Expr::value(to_db_opt(last_progress_at)),
            )
            .filter(chat_turns::Column::RequestId.eq(request_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("set liveness");
        assert_eq!(updated.rows_affected, 1, "no such turn");
    }

    /// Move every `quota_usage` row to the periods that contain `at` (daily = its UTC date,
    /// monthly = the 1st of its month): what the rows of a turn started at `at` look like.
    pub async fn relocate_quota_rows(&self, at: OffsetDateTime) {
        let conn = self.chat.db.conn().expect("db connection");
        let day = DbDate::from_ymd_opt(
            at.year(),
            u32::from(u8::from(at.month())),
            u32::from(at.day()),
        )
        .expect("date");
        let month =
            DbDate::from_ymd_opt(at.year(), u32::from(u8::from(at.month())), 1).expect("date");
        for (period, date) in [("daily", day), ("monthly", month)] {
            quota_usage::Entity::update_many()
                .col_expr(quota_usage::Column::PeriodStart, Expr::value(date))
                .filter(quota_usage::Column::PeriodType.eq(period))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .exec(&conn)
                .await
                .expect("relocate quota rows");
        }
    }

    /// `GET /v1/chats/{chat}/turns/{request_id}` as `ctx`.
    pub async fn turn_status(&self, chat: Uuid, request_id: Uuid, ctx: &SecurityContext) -> Resp {
        crate::common::get(&self.router, &self.turn_uri(chat, request_id), ctx).await
    }

    /// The message list of `chat` through the public API (as `USER_A`).
    pub async fn listed_messages(&self, chat: Uuid) -> Vec<Value> {
        let resp = self.chat.list_messages(chat, "", &ctx_a()).await;
        assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
        page_items(&resp.json())
    }

    // ── readers (unscoped: they assert what the services did) ────────────────

    pub async fn all_turns(&self) -> Vec<chat_turns::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns")
    }

    pub async fn turn_of(&self, request_id: Uuid) -> Option<chat_turns::Model> {
        self.all_turns()
            .await
            .into_iter()
            .find(|t| t.request_id == request_id)
    }

    /// The turn of `request_id` (must exist).
    pub async fn turn(&self, request_id: Uuid) -> chat_turns::Model {
        self.turn_of(request_id)
            .await
            .unwrap_or_else(|| panic!("no turn {request_id}"))
    }

    /// The turns of `chat` in `(started_at, id)` order, soft-deleted ones included.
    pub async fn turns_in(&self, chat: Uuid) -> Vec<chat_turns::Model> {
        let mut rows: Vec<_> = self
            .all_turns()
            .await
            .into_iter()
            .filter(|t| t.chat_id == chat)
            .collect();
        rows.sort_by_key(|t| (t.started_at, t.id));
        rows
    }

    /// Wait (10 s) until the turn of `request_id` reached `state`.
    pub async fn wait_turn_state(&self, request_id: Uuid, state: &str) -> chat_turns::Model {
        let mut last = None;
        for _ in 0..1000 {
            if let Some(turn) = self.turn_of(request_id).await {
                if turn.state == state {
                    return turn;
                }
                last = Some(turn.state.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("turn {request_id} never reached `{state}` (last state {last:?})");
    }

    /// Messages of `chat` in `(created_at, id)` order, soft-deleted ones included.
    pub async fn messages_in(&self, chat: Uuid) -> Vec<messages::Model> {
        let mut rows: Vec<_> = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .filter(|m| m.chat_id == chat)
            .collect();
        rows.sort_by_key(|m| (m.created_at, m.id));
        rows
    }

    /// `(role, content)` of the live (not soft-deleted) messages of `chat`, chronologically.
    pub async fn live_texts(&self, chat: Uuid) -> Vec<(String, String)> {
        self.messages_in(chat)
            .await
            .into_iter()
            .filter(|m| m.deleted_at.is_none())
            .map(|m| (m.role, m.content))
            .collect()
    }

    /// The live user message of the turn `request_id` (must exist and be unique).
    pub async fn user_message_of(&self, chat: Uuid, request_id: Uuid) -> messages::Model {
        let found: Vec<_> = self
            .messages_in(chat)
            .await
            .into_iter()
            .filter(|m| m.request_id == Some(request_id) && m.role == "user")
            .collect();
        assert_eq!(
            found.len(),
            1,
            "one user message of {request_id}: {found:?}"
        );
        found.into_iter().next().unwrap()
    }

    pub async fn all_links(&self) -> Vec<message_attachments::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        message_attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read links")
    }

    /// Attachment ids linked to `message`, sorted.
    pub async fn links_of(&self, message: Uuid) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .all_links()
            .await
            .into_iter()
            .filter(|l| l.message_id == message)
            .map(|l| l.attachment_id)
            .collect();
        ids.sort();
        ids
    }

    pub async fn summaries_of(&self, chat: Uuid) -> Vec<thread_summaries::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read summaries")
    }

    /// `(id, is_compressed)` of every message of `chat`, soft-deleted ones included.
    pub async fn compressed_flags(&self, chat: Uuid) -> Vec<(Uuid, bool)> {
        self.messages_in(chat)
            .await
            .into_iter()
            .map(|m| (m.id, m.is_compressed))
            .collect()
    }

    pub async fn usage_rows(&self) -> Vec<quota_usage::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read quota_usage")
    }

    /// The row of `USER_A` for (`bucket`, `period`) in today's / this month's period.
    pub async fn bucket(&self, bucket: &str, period: &str) -> Option<quota_usage::Model> {
        self.usage_rows().await.into_iter().find(|row| {
            row.user_id == USER_A
                && row.bucket == bucket
                && row.period_type == period
                && row.period_start == period_start_today(period)
        })
    }

    /// `(spent, reserved)` of `USER_A`'s bucket row (zeros when there is none).
    pub async fn amounts(&self, bucket: &str, period: &str) -> (i64, i64) {
        self.bucket(bucket, period).await.map_or((0, 0), |r| {
            (r.spent_credits_micro, r.reserved_credits_micro)
        })
    }

    /// Every `reserved_credits_micro` of every bucket row is back to zero.
    pub async fn assert_nothing_reserved(&self) {
        for row in self.usage_rows().await {
            assert_eq!(
                row.reserved_credits_micro, 0,
                "reserve of {}/{} ({}) is still held: {row:?}",
                row.bucket, row.period_type, row.period_start
            );
        }
    }

    /// Fingerprint of everything a rejected mutation must not change.
    pub async fn snapshot(&self) -> Snapshot {
        let mut turns: Vec<_> = self
            .all_turns()
            .await
            .into_iter()
            .map(|t| {
                (
                    t.request_id,
                    t.state,
                    t.error_code,
                    t.deleted_at,
                    t.replaced_by_request_id,
                )
            })
            .collect();
        turns.sort();
        let mut messages: Vec<_> = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .map(|m| (m.id, m.deleted_at, m.is_compressed))
            .collect();
        messages.sort();
        let mut links: Vec<_> = self
            .all_links()
            .await
            .into_iter()
            .map(|l| (l.message_id, l.attachment_id))
            .collect();
        links.sort();
        let conn = self.chat.db.conn().expect("db connection");
        let mut summaries: Vec<Uuid> = thread_summaries::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read summaries")
            .into_iter()
            .map(|s| s.chat_id)
            .collect();
        summaries.sort();
        let mut usage: Vec<_> = self
            .usage_rows()
            .await
            .into_iter()
            .map(|r| {
                (
                    r.bucket,
                    r.period_type,
                    r.spent_credits_micro,
                    r.reserved_credits_micro,
                    r.calls,
                )
            })
            .collect();
        usage.sort();
        let mut chats: Vec<_> = self
            .chat
            .all_chats()
            .await
            .into_iter()
            .map(|c| (c.id, c.updated_at))
            .collect();
        chats.sort();
        Snapshot {
            turns,
            messages,
            links,
            summaries,
            usage,
            chats,
        }
    }

    /// `chats.updated_at` of `chat`.
    pub async fn chat_updated_at(&self, chat: Uuid) -> DateTimeUtc {
        self.chat.chat_row(chat).await.expect("chat row").updated_at
    }
}

// ── assertions / small helpers ───────────────────────────────────────────────

/// 409 `aborted` with `context.reason = NOT_LATEST_TURN`.
pub fn assert_not_latest(resp: &Resp) -> Value {
    assert_conflict(resp, "NOT_LATEST_TURN")
}

/// 400 `failed_precondition` with `violations[{subject: turn_state, type: STATE}]`.
pub fn assert_turn_running(resp: &Resp) -> Value {
    let problem = assert_problem(resp, 400, "failed_precondition");
    assert_precondition(&problem, "turn_state", "STATE");
    problem
}

/// 404 `not_found` naming the `turn` resource type.
pub fn assert_turn_not_found(resp: &Resp) -> Value {
    assert_not_found(resp, "turn")
}

/// The SDK audit events of one kind, in delivery order.
pub fn audit_of(
    events: &[mini_chat_sdk::AuditEvent],
    event_type: &str,
) -> Vec<mini_chat_sdk::AuditEvent> {
    events
        .iter()
        .filter(|e| e.event_type() == event_type)
        .cloned()
        .collect()
}

/// `true` when `a` is not older than `b - slack` and not newer than `c + slack`.
pub fn within(value: OffsetDateTime, from: OffsetDateTime, to: OffsetDateTime) -> bool {
    value >= from - time::Duration::seconds(1) && value <= to + time::Duration::seconds(1)
}
