//! Send-message pipeline (DESIGN §3.6 "Send Message with Streaming
//! Response"): idempotency, parallel-turn guard, preflight, context assembly,
//! reserve transaction, provider task and SSE relay.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot, UserLimits};
use sea_orm::ActiveValue::Set;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use super::finalize::{FinalizeInput, TurnContext};
use crate::domain::authz::actions;
use crate::domain::clock;
use crate::domain::context::{self, ContextInput, ContextPlan, HistoryMessage, HistoryRole};
use crate::domain::error::{DomainError, FeatureSubject};
use crate::domain::models::{
    Citation, DoneData, DoneUsage, StreamEvent, StreamStartedData, TextSpan, ThreadSummaryInfo,
};
use crate::domain::quota::{PreflightDecision, PreflightRequest, message_tokens};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::db::entities::{attachments, chat_turns, chats, messages, thread_summaries};
use crate::infra::db::repo;
use crate::infra::db::repo::attachments::status as att_status;
use crate::infra::db::repo::turns::state;
use crate::infra::llm::types::{
    InputMessage, InputPart, InputRole, LlmEvent, LlmRequest, RawCitation, ToolSpec,
};
use crate::infra::llm::ProviderTarget;

/// Body of `messages:stream`.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
}

/// Live stream handle returned to the SSE writer.
pub struct LiveStream {
    pub rx: mpsc::Receiver<StreamEvent>,
    /// Cancelled when the client disconnects (drop guard of the SSE body).
    pub cancel: CancellationToken,
}

/// Result of a stream setup.
pub enum StreamStart {
    Replay(Vec<StreamEvent>),
    Live(LiveStream),
}

/// Inputs of a turn resolved by the setup.
pub(crate) struct TurnSetup {
    pub chat: chats::Model,
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_text: String,
    pub image_file_ids: Vec<String>,
    pub decision: PreflightDecision,
    pub limits: UserLimits,
}

/// Request-side artifacts of a prepared turn.
pub(crate) struct PreparedRequest {
    pub plan: ContextPlan,
    pub request: LlmRequest,
    pub target: ProviderTarget,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub summary: Option<thread_summaries::Model>,
}

/// Attachment facts of a chat used by the preflight.
#[derive(Debug, Default)]
pub(crate) struct ChatAttachmentFacts {
    pub ready_documents: bool,
    pub code_interpreter_file_ids: Vec<String>,
}

pub(crate) fn chat_attachment_facts(rows: &[attachments::Model]) -> ChatAttachmentFacts {
    let mut f = ChatAttachmentFacts::default();
    for a in rows {
        if a.deleted_at.is_some() || a.status != att_status::READY {
            continue;
        }
        if a.for_file_search {
            f.ready_documents = true;
        }
        if a.for_code_interpreter
            && let Some(id) = &a.provider_file_id
        {
            f.code_interpreter_file_ids.push(id.clone());
        }
    }
    f
}

/// `user` field: tenant + user UUIDs in simple form (64 chars).
#[must_use]
pub fn provider_user(tenant_id: Uuid, user_id: Uuid) -> String {
    format!("{}{}", tenant_id.simple(), user_id.simple())
}

/// Replay events of a completed turn (ADR-0010).
pub(crate) fn replay_events(
    turn: &chat_turns::Model,
    msg: &messages::Model,
    chat: &chats::Model,
) -> Vec<StreamEvent> {
    let effective = msg
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| chat.model.clone());
    let downgrade = effective != chat.model;
    vec![
        StreamEvent::StreamStarted(StreamStartedData {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        StreamEvent::Delta {
            kind: "text",
            content: msg.content.clone(),
        },
        StreamEvent::Done(Box::new(DoneData {
            usage: DoneUsage {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
            },
            effective_model: effective,
            selected_model: chat.model.clone(),
            quota_decision: if downgrade { "downgrade" } else { "allow" },
            downgrade_from: downgrade.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        })),
    ]
}

/// Validates `attachment_ids` shape (duplicates, length) before any query.
///
/// # Errors
/// `InvalidAttachment`.
pub(crate) fn validate_attachment_ids(ids: &[Uuid], max: usize) -> Result<(), DomainError> {
    if ids.len() > max {
        return Err(DomainError::InvalidAttachment("too many attachment ids".to_owned()));
    }
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Err(DomainError::InvalidAttachment("duplicate attachment id".to_owned()));
        }
    }
    Ok(())
}

/// Validates requested attachments inside the reserve transaction.
///
/// # Errors
/// `InvalidAttachment`.
pub(crate) fn validate_attachments(
    ids: &[Uuid],
    rows: &[attachments::Model],
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
) -> Result<(), DomainError> {
    for id in ids {
        let Some(a) = rows.iter().find(|a| a.id == *id) else {
            return Err(DomainError::InvalidAttachment(format!("attachment {id} not found")));
        };
        if a.tenant_id != tenant_id
            || a.chat_id != chat_id
            || a.uploaded_by_user_id != user_id
            || a.deleted_at.is_some()
            || a.status != att_status::READY
        {
            return Err(DomainError::InvalidAttachment(format!("attachment {id} is not usable")));
        }
    }
    Ok(())
}

impl MiniChatService {
    /// Builds instructions, context plan, provider request and target.
    ///
    /// # Errors
    /// `ContextBudgetExceeded`, provider resolution failure (500).
    #[allow(clippy::cognitive_complexity)]
    pub(crate) async fn prepare_request(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        setup: &TurnSetup,
    ) -> Result<PreparedRequest, DomainError> {
        let chat = &setup.chat;
        let d = &setup.decision;
        let m = &d.effective;
        let conn = self.db.conn()?;
        let summary = repo::summaries::find(&conn, chat.tenant_id, chat.id).await?;
        let boundary = repo::messages::latest_excluding_request(&conn, chat.tenant_id, chat.id, setup.request_id).await?;
        let history: Vec<HistoryMessage> = if let Some(b) = boundary {
            let frontier = summary
                .as_ref()
                .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
            let mut recent = repo::messages::recent_for_context(
                &conn,
                chat.tenant_id,
                chat.id,
                (b.created_at, b.id),
                frontier,
                u64::from(self.cfg.context.recent_messages_limit),
            )
            .await?;
            recent.reverse();
            recent
                .into_iter()
                .filter_map(|msg| {
                    let role = match msg.role.as_str() {
                        "user" => HistoryRole::User,
                        "assistant" => HistoryRole::Assistant,
                        _ => return None,
                    };
                    Some(HistoryMessage {
                        role,
                        content: msg.content,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let atts = repo::attachments::list_for_chat(&conn, chat.tenant_id, chat.id).await?;
        let facts = chat_attachment_facts(&atts);
        let storage = self.llm.resolver().storage_for(&m.provider_id, chat.tenant_id);
        let mut tools = Vec::new();
        let mut file_search = false;
        if d.gates.file_search && facts.ready_documents {
            let vs = repo::vector_stores::find(&conn, chat.tenant_id, chat.id).await?;
            if let Some(vs_id) = vs.and_then(|v| v.vector_store_id) {
                tools.push(ToolSpec::FileSearch {
                    vector_store_ids: vec![vs_id],
                    max_num_results: m.max_num_results,
                });
                file_search = true;
            }
        }
        if d.gates.web_search {
            tools.push(ToolSpec::WebSearch {
                search_context_size: m.web_search_context_size.as_str().to_owned(),
            });
        }
        if d.gates.code_interpreter && !facts.code_interpreter_file_ids.is_empty() {
            tools.push(ToolSpec::CodeInterpreter {
                file_ids: facts.code_interpreter_file_ids.clone(),
            });
        }
        let mut instructions = m.system_prompt.clone();
        let mut guards = Vec::new();
        if file_search {
            guards.push(self.cfg.context.file_search_guard.clone());
        }
        if d.gates.web_search {
            guards.push(self.cfg.context.web_search_guard.clone());
        }
        for g in guards.into_iter().filter(|g| !g.trim().is_empty()) {
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(&g);
        }
        let b = &m.estimation_budgets;
        let mut surcharges = 0_i64;
        if d.gates.file_search {
            surcharges += i64::from(b.tool_surcharge_tokens);
        }
        if d.gates.web_search {
            surcharges += i64::from(b.web_search_surcharge_tokens);
        }
        if d.gates.code_interpreter {
            surcharges += i64::from(b.code_interpreter_surcharge_tokens);
        }
        let image_count = u32::try_from(setup.image_file_ids.len()).unwrap_or(u32::MAX);
        let plan = context::assemble(ContextInput {
            model: m,
            max_output_tokens_applied: d.max_output_tokens_applied,
            instructions,
            summary: summary.as_ref().map(|s| s.summary_text.clone()),
            history,
            user_text: &setup.user_text,
            image_count,
            surcharges,
        })?;
        let target = self
            .llm
            .resolver()
            .resolve(&m.provider_id, chat.tenant_id)
            .ok_or_else(|| DomainError::Internal(format!("unknown provider '{}'", m.provider_id)))?;
        let mut input = Vec::new();
        if let Some(s) = &plan.summary_message {
            input.push(InputMessage {
                role: InputRole::User,
                parts: vec![InputPart::Text(s.clone())],
                is_current: false,
            });
        }
        for h in &plan.history {
            input.push(InputMessage {
                role: match h.role {
                    HistoryRole::User => InputRole::User,
                    HistoryRole::Assistant => InputRole::Assistant,
                },
                parts: vec![InputPart::Text(h.content.clone())],
                is_current: false,
            });
        }
        let mut parts = vec![InputPart::Text(setup.user_text.clone())];
        parts.extend(
            setup
                .image_file_ids
                .iter()
                .map(|id| InputPart::Image { file_id: id.clone() }),
        );
        input.push(InputMessage {
            role: InputRole::User,
            parts,
            is_current: true,
        });
        let feature = if tools.is_empty() {
            "none".to_owned()
        } else {
            tools.iter().map(ToolSpec::feature).collect::<Vec<_>>().join("+")
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".to_owned(), chat.tenant_id.to_string().into());
        metadata.insert("user_id".to_owned(), ctx.subject_id().to_string().into());
        metadata.insert("chat_id".to_owned(), chat.id.to_string().into());
        metadata.insert("request_type".to_owned(), "chat".into());
        metadata.insert("feature".to_owned(), feature.into());
        let request = LlmRequest {
            model: m.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input,
            tools,
            max_output_tokens: u32::try_from(d.max_output_tokens_applied).unwrap_or(u32::MAX),
            max_tool_calls: Some(m.max_tool_calls),
            user: provider_user(chat.tenant_id, ctx.subject_id()),
            metadata,
            api_params: m.general_config.api_params.clone(),
            stream: true,
        };
        let mut file_map = HashMap::new();
        if file_search {
            for a in &atts {
                if a.deleted_at.is_none()
                    && a.status == att_status::READY
                    && let Some(pid) = &a.provider_file_id
                {
                    file_map.insert(pid.clone(), (a.id, a.filename.clone()));
                }
            }
        }
        let _ = storage;
        let summary_kept = plan.summary_message.is_some();
        Ok(PreparedRequest {
            plan,
            request,
            target,
            file_map,
            summary: summary.filter(|_| summary_kept),
        })
    }

    /// Image facts of requested attachment ids: provider file ids of images.
    pub(crate) fn image_file_ids(rows: &[attachments::Model], ids: &[Uuid]) -> Vec<String> {
        ids.iter()
            .filter_map(|id| rows.iter().find(|a| a.id == *id))
            .filter(|a| a.attachment_kind == crate::domain::models::attachment_kind::IMAGE)
            .filter_map(|a| a.provider_file_id.clone())
            .collect()
    }

    /// Image guards after the cascade (kill switch, vision).
    ///
    /// # Errors
    /// `FeatureDisabled(Images)` / `VisionNotSupported`.
    pub(crate) fn image_guards(
        snapshot: &PolicySnapshot,
        effective: &ModelCatalogEntry,
        image_count: usize,
    ) -> Result<(), DomainError> {
        if image_count == 0 {
            return Ok(());
        }
        if snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled(FeatureSubject::Images));
        }
        if !effective.supports_vision() {
            return Err(DomainError::VisionNotSupported);
        }
        Ok(())
    }

    /// Sends a message; returns a replay or a live stream.
    ///
    /// # Errors
    /// Pre-stream errors (JSON Problem responses).
    #[allow(clippy::too_many_lines)]
    pub async fn send_message(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> Result<StreamStart, DomainError> {
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let rag = &self.cfg.rag;
        validate_attachment_ids(
            &req.attachment_ids,
            usize::try_from(rag.max_documents_per_chat + rag.max_images_per_message).unwrap_or(usize::MAX),
        )?;
        let (_, chat) = self.authorized_chat(ctx, actions::SEND_MESSAGE, chat_id).await?;
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let conn = self.db.conn()?;
        // 1. Idempotency (before the parallel-turn guard).
        if let Some(turn) = repo::turns::find_by_request(&conn, chat.tenant_id, chat.id, request_id).await? {
            if turn.deleted_at.is_none() && turn.state == state::COMPLETED {
                let msg = match turn.assistant_message_id {
                    Some(id) => repo::messages::find_by_id(&conn, id).await?,
                    None => None,
                };
                let msg = msg.ok_or_else(|| DomainError::internal("completed turn without assistant message"))?;
                return Ok(StreamStart::Replay(replay_events(&turn, &msg, &chat)));
            }
            return Err(DomainError::RequestIdConflict);
        }
        // 2. Parallel turn guard.
        if repo::turns::find_running(&conn, chat.tenant_id, chat.id).await?.is_some() {
            return Err(DomainError::TurnAlreadyRunning);
        }
        // 3. Model of the chat.
        let user = ctx.subject_id();
        let snapshot = self.policy.current_snapshot(user).await?;
        if snapshot.find(&chat.model).is_none() {
            return Err(DomainError::InvalidModel);
        }
        let prior = repo::messages::latest_assistant_with_usage(&conn, chat.tenant_id, chat.id)
            .await?
            .map_or(0, |m| m.input_tokens + m.output_tokens);
        let requested = repo::attachments::find_many(&conn, chat.tenant_id, chat.id, &req.attachment_ids).await?;
        let image_file_ids = Self::image_file_ids(&requested, &req.attachment_ids);
        let image_count = requested
            .iter()
            .filter(|a| a.attachment_kind == crate::domain::models::attachment_kind::IMAGE)
            .count();
        if image_count > usize::try_from(rag.max_images_per_message).unwrap_or(usize::MAX) {
            return Err(DomainError::TooManyImages);
        }
        let all_atts = repo::attachments::list_for_chat(&conn, chat.tenant_id, chat.id).await?;
        let facts = chat_attachment_facts(&all_atts);
        // 4. Kill switch before the cascade.
        if req.web_search_enabled && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled(FeatureSubject::WebSearch));
        }
        // 5. Quota preflight.
        let limits = self.policy.user_limits(user, snapshot.policy_version).await?;
        let preq = PreflightRequest {
            selected_model: &chat.model,
            message_bytes: req.content.len(),
            prior_context_tokens: prior,
            image_count: u32::try_from(image_count).unwrap_or(u32::MAX),
            has_ready_documents: facts.ready_documents,
            has_ready_code_interpreter: !facts.code_interpreter_file_ids.is_empty(),
            web_search_requested: req.web_search_enabled,
        };
        let decision = match self
            .quota
            .preflight(&conn, chat.tenant_id, user, &snapshot, &limits, &preq, clock::now())
            .await
        {
            Ok(d) => {
                self.metrics.inc(
                    "quota_preflight",
                    1,
                    &[("decision", d.decision_str().to_owned()), ("model", d.effective.id.clone()), ("tier", d.effective.tier.as_str().to_owned())],
                );
                d
            }
            Err(e) => {
                self.metrics.inc("quota_preflight", 1, &[("decision", "reject".to_owned()), ("model", chat.model.clone()), ("tier", String::new())]);
                return Err(e);
            }
        };
        #[allow(clippy::cast_precision_loss)] // metrics value only
        self.metrics.record("quota_estimated_tokens", decision.reserve_tokens as f64, &[]);
        // 6. Input limit and image guards on the effective model.
        let eff = &decision.effective;
        if eff.max_input_tokens > 0 && message_tokens(&req.content, eff) > i64::from(eff.max_input_tokens) {
            return Err(DomainError::InputTooLong);
        }
        Self::image_guards(&snapshot, eff, image_count)?;
        // 7. Context assembly and provider resolution.
        let setup = TurnSetup {
            chat: chat.clone(),
            request_id,
            turn_id: Uuid::new_v4(),
            user_text: req.content.clone(),
            image_file_ids,
            decision,
            limits,
        };
        let prepared = self.prepare_request(ctx, &setup).await?;
        // 8. Reserve transaction.
        self.reserve_and_create_turn(ctx, &setup, &req).await?;
        Ok(StreamStart::Live(self.spawn_turn(ctx.clone(), setup, prepared)))
    }

    async fn reserve_and_create_turn(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        setup: &TurnSetup,
        req: &SendRequest,
    ) -> Result<(), DomainError> {
        let svc = Arc::clone(self);
        let tenant_id = setup.chat.tenant_id;
        let chat_id = setup.chat.id;
        let user_id = ctx.subject_id();
        let decision = setup.decision.clone();
        let limits = setup.limits.clone();
        let request_id = setup.request_id;
        let turn_id = setup.turn_id;
        let content = req.content.clone();
        let att_ids = req.attachment_ids.clone();
        let web_search = req.web_search_enabled;
        let floor = decision.minimal_generation_floor_applied;
        let res = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    svc.quota.reserve(tx, tenant_id, user_id, &decision, &limits, now).await?;
                    let user_msg_id = Uuid::new_v4();
                    repo::messages::insert(tx, tenant_id, new_message(user_msg_id, tenant_id, chat_id, request_id, "user", &content, None, now)).await?;
                    repo::chats::touch(tx, tenant_id, chat_id, now).await?;
                    if !att_ids.is_empty() {
                        let rows = repo::attachments::find_many(tx, tenant_id, chat_id, &att_ids).await?;
                        validate_attachments(&att_ids, &rows, tenant_id, user_id, chat_id)?;
                        repo::messages::link_attachments(tx, tenant_id, chat_id, user_msg_id, &att_ids, now).await?;
                    }
                    let am = chat_turns::ActiveModel {
                        id: Set(turn_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(request_id),
                        requester_type: Set("user".to_owned()),
                        requester_user_id: Set(Some(user_id)),
                        state: Set(state::RUNNING.to_owned()),
                        provider_name: Set(None),
                        provider_response_id: Set(None),
                        assistant_message_id: Set(None),
                        error_code: Set(None),
                        reserve_tokens: Set(Some(decision.reserve_tokens)),
                        max_output_tokens_applied: Set(Some(i32::try_from(decision.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        reserved_credits_micro: Set(Some(decision.reserved_credits_micro)),
                        policy_version_applied: Set(Some(i64::try_from(decision.policy_version).unwrap_or(i64::MAX))),
                        effective_model: Set(Some(decision.effective.id.clone())),
                        minimal_generation_floor_applied: Set(Some(i32::try_from(floor).unwrap_or(i32::MAX))),
                        error_detail: Set(None),
                        deleted_at: Set(None),
                        replaced_by_request_id: Set(None),
                        started_at: Set(now),
                        last_progress_at: Set(Some(now)),
                        web_search_enabled: Set(web_search),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(now),
                    };
                    repo::turns::insert(tx, tenant_id, am).await?;
                    Ok(((), toolkit_db::outbox::Wake::empty()))
                })
            })
            .await;
        match res {
            Ok(()) => {
                self.metrics.inc("quota_reserve", 2, &[("period", "all".to_owned())]);
                Ok(())
            }
            Err(DomainError::UniqueViolation) => {
                let conn = self.db.conn()?;
                if repo::turns::find_by_request(&conn, tenant_id, chat_id, request_id).await?.is_some() {
                    Err(DomainError::RequestIdConflict)
                } else {
                    Err(DomainError::TurnAlreadyRunning)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Spawns the provider task and returns the live stream.
    pub(crate) fn spawn_turn(
        self: &Arc<Self>,
        ctx: SecurityContext,
        setup: TurnSetup,
        prepared: PreparedRequest,
    ) -> LiveStream {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let svc = Arc::clone(self);
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            svc.run_turn(ctx, setup, prepared, tx, task_cancel).await;
        });
        LiveStream { rx, cancel }
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::items_after_statements)]
    async fn run_turn(
        self: Arc<Self>,
        ctx: SecurityContext,
        setup: TurnSetup,
        prepared: PreparedRequest,
        tx: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let started = Instant::now();
        let message_id = Uuid::new_v4();
        let tctx = TurnContext::from_setup(&ctx, &setup, message_id);
        let provider = prepared.target.provider_id.clone();
        let model = setup.decision.effective.id.clone();
        let labels = [("provider", provider.clone()), ("model", model.clone())];
        self.metrics.inc("stream_started", 1, &labels);
        self.metrics.updown("active_streams", 1);
        let summary_info = prepared.summary.as_ref().map(|s| ThreadSummaryInfo {
            token_estimate: u32::try_from(s.token_estimate).unwrap_or(0),
        });
        let mut disconnected = tx
            .send(StreamEvent::StreamStarted(StreamStartedData {
                request_id: setup.request_id,
                message_id,
                is_new_turn: true,
                thread_summary_applied: summary_info,
            }))
            .await
            .is_err();

        let mut text = String::new();
        let mut citations: Vec<Citation> = Vec::new();
        let mut ws_started = 0_u32;
        let mut ci_started = 0_u32;
        let mut ws_done = 0_i32;
        let mut ci_done = 0_i32;
        let mut fs_done = 0_i32;
        let mut ttft: Option<u64> = None;
        let mut content_started = false;
        let mut last_touch = Instant::now();
        let ping_every = Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds));
        let qcfg = self.quota.config().clone();

        enum Outcome {
            Completed {
                usage: Option<mini_chat_sdk::UsageTokens>,
                response_id: Option<String>,
                incomplete: Option<String>,
            },
            Failed {
                code: String,
                message: String,
                usage: Option<mini_chat_sdk::UsageTokens>,
            },
            Cancelled,
        }

        let outcome = if disconnected {
            Outcome::Cancelled
        } else {
            let start = tokio::select! {
                r = self.llm.stream_chat(&prepared.target, &prepared.request, &ctx) => Some(r),
                () = cancel.cancelled() => None,
            };
            match start {
                None => Outcome::Cancelled,
                Some(Err(f)) => Outcome::Failed {
                    code: f.kind.code().to_owned(),
                    message: sanitize_provider_message(&f.message),
                    usage: f.usage,
                },
                Some(Ok(mut stream)) => {
                    let mut ping_deadline = tokio::time::Instant::now() + ping_every;
                    loop {
                        let ping = tokio::time::sleep_until(ping_deadline);
                        let ev = tokio::select! {
                            biased;
                            () = cancel.cancelled() => { break Outcome::Cancelled; }
                            ev = stream.next() => ev,
                            () = ping, if !content_started => {
                                if tx.send(StreamEvent::Ping).await.is_err() {
                                    disconnected = true;
                                    break Outcome::Cancelled;
                                }
                                ping_deadline = tokio::time::Instant::now() + ping_every;
                                continue;
                            }
                        };
                        let Some(ev) = ev else {
                            break Outcome::Failed {
                                code: "provider_error".to_owned(),
                                message: "Provider stream ended unexpectedly".to_owned(),
                                usage: None,
                            };
                        };
                        let mut out: Option<StreamEvent> = None;
                        let mut progress = false;
                        match ev {
                            LlmEvent::TextDelta(t) => {
                                if ttft.is_none() {
                                    ttft = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                                }
                                content_started = true;
                                text.push_str(&t);
                                out = Some(StreamEvent::Delta { kind: "text", content: t });
                                progress = true;
                            }
                            LlmEvent::ReasoningDelta(t) => {
                                content_started = true;
                                out = Some(StreamEvent::Delta { kind: "reasoning", content: t });
                                progress = true;
                            }
                            LlmEvent::ToolStart { name, details } => {
                                content_started = true;
                                if name == "web_search" {
                                    ws_started += 1;
                                    if ws_started > qcfg.web_search_max_calls_per_message {
                                        break Outcome::Failed {
                                            code: "web_search_calls_exceeded".to_owned(),
                                            message: "Web search call limit per message exceeded".to_owned(),
                                            usage: None,
                                        };
                                    }
                                }
                                if name == "code_interpreter" {
                                    ci_started += 1;
                                    if ci_started > qcfg.code_interpreter_max_calls_per_message {
                                        break Outcome::Failed {
                                            code: "code_interpreter_calls_exceeded".to_owned(),
                                            message: "Code interpreter call limit per message exceeded".to_owned(),
                                            usage: None,
                                        };
                                    }
                                }
                                out = Some(StreamEvent::Tool { phase: "start", name, details });
                                progress = true;
                            }
                            LlmEvent::ToolDone { name, details } => {
                                content_started = true;
                                match name.as_str() {
                                    "web_search" => ws_done += 1,
                                    "code_interpreter" => ci_done += 1,
                                    "file_search" => fs_done += 1,
                                    _ => {}
                                }
                                out = Some(StreamEvent::Tool { phase: "done", name, details });
                                progress = true;
                            }
                            LlmEvent::Citation(c) => {
                                if let Some(c) = map_citation(c, &prepared.file_map) {
                                    citations.push(c);
                                }
                            }
                            LlmEvent::Completed { usage, response_id, incomplete_reason } => {
                                break Outcome::Completed { usage, response_id, incomplete: incomplete_reason };
                            }
                            LlmEvent::Failed(f) => {
                                break Outcome::Failed {
                                    code: f.kind.code().to_owned(),
                                    message: sanitize_provider_message(&f.message),
                                    usage: f.usage,
                                };
                            }
                        }
                        if let Some(e) = out
                            && tx.send(e).await.is_err()
                        {
                            disconnected = true;
                            break Outcome::Cancelled;
                        }
                        if progress && last_touch.elapsed() >= Duration::from_secs(30) {
                            last_touch = Instant::now();
                            if let Ok(conn) = self.db.conn() {
                                repo::turns::touch_progress(
                                    &conn,
                                    tctx.tenant_id,
                                    tctx.turn_id,
                                    Some((ws_done, ci_done, fs_done)),
                                    clock::now(),
                                )
                                .await
                                .ok();
                            }
                        }
                    }
                }
            }
        };
        if cancel.is_cancelled() {
            disconnected = true;
        }
        let counters = (ws_done, ci_done, fs_done);
        let total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let input = FinalizeInput {
            text: text.clone(),
            counters,
            ttft_ms: ttft,
            total_ms,
            plan: Some(prepared.plan.clone()),
            has_summary: prepared.summary.is_some(),
        };
        match outcome {
            Outcome::Completed {
                usage,
                response_id,
                incomplete,
            } => {
                if let Some(reason) = &incomplete {
                    tracing::warn!(reason = %reason, "stream incomplete");
                    self.metrics.inc("stream_incomplete", 1, &[("provider", provider.clone()), ("model", model.clone()), ("reason", reason.clone())]);
                }
                if !citations.is_empty() && !disconnected {
                    tx.send(StreamEvent::Citations(citations)).await.ok();
                }
                match self.finalize_completed(&tctx, input, usage, response_id).await {
                    Ok(Some(done)) => {
                        self.metrics.inc("stream_completed", 1, &labels);
                        tx.send(StreamEvent::Done(Box::new(done))).await.ok();
                    }
                    Ok(None) => {}
                    Err(code) => {
                        self.metrics.inc("stream_failed", 1, &[("provider", provider.clone()), ("model", model.clone()), ("error_code", code.to_owned())]);
                        tx
                            .send(StreamEvent::Error {
                                code: code.to_owned(),
                                message: if code == "message_persistence_failed" {
                                    "The answer could not be saved".to_owned()
                                } else {
                                    "The turn could not be finalized".to_owned()
                                },
                            })
                            .await
                            .ok();
                    }
                }
            }
            Outcome::Failed { code, message, usage } => {
                self.metrics.inc("stream_failed", 1, &[("provider", provider.clone()), ("model", model.clone()), ("error_code", code.clone())]);
                let won = self.finalize_failed(&tctx, input, &code, &message, usage).await;
                if won != Some(false) {
                    tx.send(StreamEvent::Error { code, message }).await.ok();
                }
            }
            Outcome::Cancelled => {
                self.metrics.inc("stream_disconnected", 1, &[("stage", if content_started { "mid_stream" } else { "before_first_token" }.to_owned())]);
                self.metrics.inc("cancel_requested", 1, &[("trigger", "disconnect".to_owned())]);
                self.finalize_cancelled(&tctx, input).await;
            }
        }
        self.metrics.updown("active_streams", -1);
        #[allow(clippy::cast_precision_loss)] // metrics value only
        self.metrics.record("stream_total_latency_ms", total_ms as f64, &labels);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn new_message(
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    role: &str,
    content: &str,
    model: Option<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> messages::ActiveModel {
    messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(Some(request_id)),
        role: Set(role.to_owned()),
        content: Set(content.to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(model),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(None),
    }
}

fn map_citation(c: RawCitation, file_map: &HashMap<String, (Uuid, String)>) -> Option<Citation> {
    match c {
        RawCitation::Web {
            url,
            title,
            snippet,
            span,
        } => Some(Citation {
            source: "web",
            title,
            url: Some(url),
            attachment_id: None,
            snippet,
            span: span.map(|(start, end)| TextSpan { start, end }),
        }),
        RawCitation::File { file_id, .. } => file_map.get(&file_id).map(|(id, name)| Citation {
            source: "file",
            title: name.clone(),
            url: None,
            attachment_id: Some(*id),
            snippet: String::new(),
            span: None,
        }),
    }
}
