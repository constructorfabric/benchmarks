//! Send-message pipeline: idempotency/parallel guards, preflight, reserve transaction, provider
//! task with SSE relay, and the side-effect-free replay path (DESIGN §3.3, §3.6, §4).

use crate::infra::db::WriteTransaction;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::chats::{load_owned_chat, tenant_scope};
use super::finalize::{FinalizeInput, Terminal, TurnMeta};
use super::quota::{
    PreflightDecision, PreflightInput, QuotaDecision, ReserveSpec, ToolGates, reserve_in_tx,
};
use super::{Core, now, provider_user_field};
use crate::domain::authz::{self, actions};
use crate::domain::context::{self, ContextInputs, ContextPlan, HistoryMessage};
use crate::domain::error::{DomainError, Resource};
use crate::domain::quota_math::estimate_text_tokens;
use crate::infra::db::entities::{
    attachment, chat, message, message_attachment, thread_summary, turn, vector_store,
};
use crate::infra::llm::provider::ResolvedProvider;
use crate::infra::llm::responses::{self, ChatRequest, InputRole, LlmEvent, ToolSet};

/// One SSE event (name + JSON data).
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: &'static str,
    pub data: Value,
}

impl SseEvent {
    #[must_use]
    pub fn new(event: &'static str, data: Value) -> Self {
        Self { event, data }
    }

    #[must_use]
    pub fn error(code: &str, message: &str) -> Self {
        Self::new("error", json!({"code": code, "message": message}))
    }
}

/// A live stream: events plus the token cancelled when the client goes away.
pub struct LiveStream {
    pub rx: mpsc::Receiver<SseEvent>,
    pub cancel: CancellationToken,
}

/// Outcome of the stream setup.
pub enum StreamStart {
    Replay(Vec<SseEvent>),
    Live(LiveStream),
}

/// Body of `messages:stream`.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Validates message content (400 `EMPTY_CONTENT`).
///
/// # Errors
/// Empty or whitespace-only content.
pub fn validate_content(content: &str) -> Result<(), DomainError> {
    if content.trim().is_empty() {
        return Err(DomainError::invalid(
            Resource::Chat,
            "content",
            "EMPTY_CONTENT",
            "content must not be empty",
        ));
    }
    Ok(())
}

#[must_use]
pub fn invalid_attachment(desc: &str) -> DomainError {
    DomainError::invalid(Resource::Chat, "attachment", "invalid_attachment", desc)
}

#[must_use]
pub fn is_image_mime(ct: &str) -> bool {
    matches!(ct, "image/png" | "image/jpeg" | "image/webp" | "image/gif")
}

/// Attachment state of a chat relevant to tools.
#[derive(Debug, Clone, Default)]
pub struct ChatAttachmentState {
    pub ready_documents: bool,
    pub code_interpreter_file_ids: Vec<String>,
    pub vector_store_id: Option<String>,
    /// `provider_file_id` -> (`attachment_id`, filename) for citation mapping.
    pub citation_map: HashMap<String, (Uuid, String)>,
}

/// Loads the attachment state of a chat.
///
/// # Errors
/// DB errors.
pub async fn load_attachment_state(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<ChatAttachmentState, DomainError> {
    let scope = tenant_scope(tenant_id);
    let rows = attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Status.eq("ready"))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&scope)
        .all(db)
        .await?;
    let mut st = ChatAttachmentState::default();
    for a in &rows {
        if a.for_file_search && a.attachment_kind == "document" {
            st.ready_documents = true;
        }
        if a.for_code_interpreter
            && let Some(f) = &a.provider_file_id
        {
            st.code_interpreter_file_ids.push(f.clone());
        }
        if let Some(f) = &a.provider_file_id {
            st.citation_map
                .insert(f.clone(), (a.id, a.filename.clone()));
        }
    }
    st.vector_store_id = vector_store::Entity::find()
        .filter(vector_store::Column::ChatId.eq(chat_id))
        .filter(vector_store::Column::TenantId.eq(tenant_id))
        .secure()
        .scope_with(&scope)
        .one(db)
        .await?
        .and_then(|v| v.vector_store_id);
    if st.vector_store_id.is_none() {
        st.ready_documents = false;
    }
    Ok(st)
}

/// `input_tokens + output_tokens` of the latest non-deleted assistant message with usage.
///
/// # Errors
/// DB errors.
pub async fn prior_context_tokens(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<i64, DomainError> {
    let row = message::Entity::find()
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::Role.eq("assistant"))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(message::Column::InputTokens.gt(0))
                        .add(message::Column::OutputTokens.gt(0)),
                ),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(db)
        .await?;
    Ok(row.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
}

/// Recent non-compressed messages after the summary frontier, chronological.
///
/// # Errors
/// DB errors.
pub async fn load_history(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    frontier: Option<(OffsetDateTime, Uuid)>,
    limit: u32,
    exclude_request: Option<Uuid>,
) -> Result<Vec<HistoryMessage>, DomainError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut cond = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::RequestId.is_not_null())
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(message::Column::Role.ne("system"));
    if let Some((ts, id)) = frontier {
        cond = cond.add(
            Condition::any().add(message::Column::CreatedAt.gt(ts)).add(
                Condition::all()
                    .add(message::Column::CreatedAt.eq(ts))
                    .add(message::Column::Id.gt(id)),
            ),
        );
    }
    if let Some(r) = exclude_request {
        cond = cond.add(message::Column::RequestId.ne(r));
    }
    let mut rows = message::Entity::find()
        .filter(cond)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(u64::from(limit))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(db)
        .await?;
    rows.reverse();
    Ok(rows
        .into_iter()
        .map(|m| HistoryMessage {
            role: if m.role == "assistant" {
                InputRole::Assistant
            } else {
                InputRole::User
            },
            content: m.content,
        })
        .collect())
}

/// Loads the chat's thread summary.
///
/// # Errors
/// DB errors.
pub async fn load_summary(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<thread_summary::Model>, DomainError> {
    Ok(thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(db)
        .await?)
}

/// Everything prepared before the provider call.
pub struct PreparedTurn {
    pub decision: PreflightDecision,
    pub plan: ContextPlan,
    pub provider: ResolvedProvider,
    pub request: ChatRequest,
    pub attachments: ChatAttachmentState,
    pub summary_token_estimate: Option<i32>,
    pub summary_exists: bool,
}

/// Inputs of `prepare_turn` (shared by send / retry / edit).
pub struct PrepareInput<'a> {
    pub ctx: &'a SecurityContext,
    pub chat: &'a chat::Model,
    pub content: &'a str,
    pub image_file_ids: Vec<String>,
    pub web_search: bool,
    pub request_id: Uuid,
    pub exclude_request_from_history: Option<Uuid>,
}

/// Validates the image guards against the effective model.
///
/// # Errors
/// 400 `FEATURE_DISABLED` images / `VISION_NOT_SUPPORTED`.
pub fn image_guards(
    snapshot: &PolicySnapshot,
    effective: &ModelCatalogEntry,
    images: usize,
) -> Result<(), DomainError> {
    if images == 0 {
        return Ok(());
    }
    if snapshot.kill_switches.disable_images {
        return Err(DomainError::feature_disabled("images"));
    }
    if !effective.supports_vision() {
        return Err(DomainError::invalid(
            Resource::Chat,
            "content_type",
            "VISION_NOT_SUPPORTED",
            "the effective model does not support image input",
        ));
    }
    Ok(())
}

impl Core {
    /// Quota preflight + input checks for a turn (no writes).
    ///
    /// # Errors
    /// 400 / 429 / 500 pre-provider errors.
    pub async fn preflight_turn(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        content: &str,
        image_count: usize,
        web_search: bool,
    ) -> Result<(PreflightDecision, ChatAttachmentState), DomainError> {
        let conn = self.db.conn()?;
        let attachments = load_attachment_state(&conn, chat.tenant_id, chat.id).await?;
        let prior = prior_context_tokens(&conn, chat.tenant_id, chat.id).await?;
        if image_count > self.cfg.rag.max_images_per_message as usize {
            return Err(DomainError::out_of_range(
                Resource::Chat,
                "image_count",
                "TOO_MANY_IMAGES",
                format!(
                    "at most {} images per message",
                    self.cfg.rag.max_images_per_message
                ),
            ));
        }
        let input = PreflightInput {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            selected_model: chat.model.clone(),
            message_bytes: content.len(),
            image_count: u32::try_from(image_count).unwrap_or(u32::MAX),
            prior_context_tokens: prior,
            gates: ToolGates {
                has_ready_documents: attachments.ready_documents,
                has_ready_code_interpreter_files: !attachments.code_interpreter_file_ids.is_empty(),
                web_search_requested: web_search,
            },
        };
        let decision = self.preflight(&input).await?;
        let eff = &decision.effective;
        if eff.max_input_tokens > 0
            && estimate_text_tokens(content.len(), &eff.estimation_budgets)
                > i64::from(eff.max_input_tokens)
        {
            return Err(DomainError::out_of_range(
                Resource::Chat,
                "content",
                "INPUT_TOO_LONG",
                "message exceeds the model's maximum input tokens",
            ));
        }
        image_guards(&decision.snapshot, eff, image_count)?;
        Ok((decision, attachments))
    }

    /// Context assembly, provider resolution and request construction (after preflight).
    ///
    /// # Errors
    /// 400 `CONTEXT_BUDGET_EXCEEDED`, 500 provider resolution.
    pub async fn build_turn_request(
        &self,
        inp: &PrepareInput<'_>,
        decision: PreflightDecision,
        attachments: ChatAttachmentState,
    ) -> Result<PreparedTurn, DomainError> {
        let conn = self.db.conn()?;
        let summary = load_summary(&conn, inp.chat.tenant_id, inp.chat.id).await?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let history = load_history(
            &conn,
            inp.chat.tenant_id,
            inp.chat.id,
            frontier,
            self.cfg.context.recent_messages_limit,
            inp.exclude_request_from_history,
        )
        .await?;
        let plan = context::assemble(ContextInputs {
            model: &decision.effective,
            max_output_tokens_applied: decision.max_output_tokens_applied,
            tools: decision.tools,
            web_search_guard: &self.cfg.context.web_search_guard,
            file_search_guard: &self.cfg.context.file_search_guard,
            summary: summary.as_ref().map(|s| s.summary_text.as_str()),
            history,
            user_text: inp.content,
            image_file_ids: inp.image_file_ids.clone(),
        })?;
        let provider = self
            .providers
            .resolve(&decision.effective.provider_id, inp.ctx.subject_tenant_id())?;
        let mut tools = ToolSet::default();
        if decision.tools.file_search
            && let Some(vs) = &attachments.vector_store_id
        {
            tools.file_search = Some((vec![vs.clone()], decision.effective.max_num_results));
        }
        if decision.tools.web_search {
            tools.web_search = Some(decision.effective.web_search_context_size);
        }
        if decision.tools.code_interpreter {
            tools.code_interpreter = Some(attachments.code_interpreter_file_ids.clone());
        }
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "tenant_id".into(),
            json!(inp.ctx.subject_tenant_id().to_string()),
        );
        metadata.insert("user_id".into(), json!(inp.ctx.subject_id().to_string()));
        metadata.insert("chat_id".into(), json!(inp.chat.id.to_string()));
        metadata.insert("request_type".into(), json!("chat"));
        metadata.insert("feature".into(), json!(tools.feature_label()));
        let request = ChatRequest {
            provider_model_id: decision.effective.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input: plan.input.clone(),
            tools,
            max_output_tokens: decision.max_output_tokens_applied,
            max_tool_calls: decision.effective.max_tool_calls,
            user: provider_user_field(inp.ctx.subject_tenant_id(), inp.ctx.subject_id()),
            metadata,
            api_params: decision.effective.general_config.api_params.clone(),
            stream: true,
        };
        let summary_token_estimate = if plan.summary_included {
            summary.as_ref().map(|s| s.token_estimate)
        } else {
            None
        };
        Ok(PreparedTurn {
            decision,
            plan,
            provider,
            request,
            attachments,
            summary_token_estimate,
            summary_exists: summary.is_some(),
        })
    }

    /// `POST /v1/chats/{id}/messages:stream` setup. Returns a replay or a live stream.
    ///
    /// # Errors
    /// Pre-stream JSON errors (400/403/404/409/429/500/503).
    pub async fn start_send(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> Result<StreamStart, DomainError> {
        validate_content(&req.content)?;
        let max_ids =
            (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if req.attachment_ids.len() > max_ids {
            return Err(invalid_attachment("too many attachment ids"));
        }
        let mut seen = HashSet::new();
        if !req.attachment_ids.iter().all(|a| seen.insert(*a)) {
            return Err(invalid_attachment("duplicate attachment ids"));
        }
        let scope =
            authz::chat_scope(&self.enforcer, ctx, actions::SEND_MESSAGE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_owned_chat(&conn, &scope, ctx, chat_id).await?;
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);

        // 1. Idempotency (highest priority), 2. parallel turn guard.
        if let Some(t) = find_turn(&conn, chat.tenant_id, chat_id, request_id).await? {
            if t.state == "completed" && t.deleted_at.is_none() {
                self.metrics.replay();
                return Ok(StreamStart::Replay(self.replay_events(&chat, &t).await?));
            }
            return Err(request_id_conflict());
        }
        if has_running_turn(&conn, chat.tenant_id, chat_id).await? {
            return Err(turn_already_running());
        }
        // Image count of the referenced attachments (validated fully in the reserve transaction).
        let referenced =
            load_referenced_attachments(&conn, chat.tenant_id, chat_id, &req.attachment_ids)
                .await?;
        let image_count = referenced
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .count();

        // The chat's model must still exist in the catalog.
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        if snapshot.find_model(&chat.model).is_none() {
            return Err(DomainError::invalid_model());
        }
        let (decision, attachments) = self
            .preflight_turn(ctx, &chat, &req.content, image_count, req.web_search)
            .await?;
        // Images in the order the client listed them.
        let image_ids: Vec<String> = req
            .attachment_ids
            .iter()
            .filter_map(|id| referenced.iter().find(|a| a.id == *id))
            .filter(|a| a.attachment_kind == "image")
            .filter_map(|a| a.provider_file_id.clone())
            .collect();
        let prep_input = PrepareInput {
            ctx,
            chat: &chat,
            content: &req.content,
            image_file_ids: image_ids,
            web_search: req.web_search,
            request_id,
            exclude_request_from_history: None,
        };
        let prepared = self
            .build_turn_request(&prep_input, decision, attachments)
            .await?;

        // Single reserve transaction.
        let turn_id = Uuid::new_v4();
        let user_msg_id = Uuid::new_v4();
        let tx_in = ReserveTx {
            tenant_id: chat.tenant_id,
            user_id: ctx.subject_id(),
            chat_id,
            turn_id,
            request_id,
            user_msg_id,
            content: req.content.clone(),
            attachment_ids: req.attachment_ids.clone(),
            web_search: req.web_search,
            decision: prepared.decision.clone(),
            mutation: None,
        };
        let core = Arc::clone(self);
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move { core.reserve_turn_tx(tx, tx_in).await })
            })
            .await;
        let user_msg_created = match res {
            Ok(ts) => ts,
            Err(e) => {
                return Err(self
                    .classify_insert_conflict(e, chat.tenant_id, chat_id, request_id)
                    .await);
            }
        };
        Ok(StreamStart::Live(self.spawn_turn(
            ctx.clone(),
            &chat,
            turn_id,
            request_id,
            user_msg_id,
            user_msg_created,
            prepared,
        )))
    }

    async fn classify_insert_conflict(
        &self,
        e: DomainError,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainError {
        let is_unique = matches!(&e, DomainError::Internal(m) if m.contains("UNIQUE") || m.contains("unique") || m.contains("duplicate"));
        if !is_unique {
            return e;
        }
        let Ok(conn) = self.db.conn() else { return e };
        match find_turn(&conn, tenant_id, chat_id, request_id).await {
            Ok(Some(_)) => request_id_conflict(),
            _ => turn_already_running(),
        }
    }

    /// Reserve transaction body: quota reserve, user message, attachments, running turn.
    ///
    /// # Errors
    /// 429 re-check, 400 invalid attachment, DB unique violations.
    pub async fn reserve_turn_tx(
        &self,
        tx: &toolkit_db::DbTx<'_>,
        r: ReserveTx,
    ) -> Result<OffsetDateTime, DomainError> {
        let d = &r.decision;
        reserve_in_tx(
            tx,
            ReserveSpec {
                tenant_id: r.tenant_id,
                user_id: r.user_id,
                tier: d.tier(),
                reserved_credits_micro: d.reserved_credits_micro,
                daily_start: d.daily_start,
                monthly_start: d.monthly_start,
            },
            &d.limits,
        )
        .await?;
        let scope = tenant_scope(r.tenant_id);
        let ts = now();
        if r.mutation.is_none() {
            insert_user_message(
                tx,
                r.tenant_id,
                r.chat_id,
                r.user_msg_id,
                r.request_id,
                &r.content,
                ts,
            )
            .await?;
            chat::Entity::update_many()
                .col_expr(chat::Column::UpdatedAt, Expr::value(ts))
                .filter(chat::Column::Id.eq(r.chat_id))
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?;
            link_attachments(
                tx,
                r.tenant_id,
                r.user_id,
                r.chat_id,
                r.user_msg_id,
                &r.attachment_ids,
                ts,
            )
            .await?;
            insert_running_turn(tx, &r, Some(d), ts).await?;
        } else {
            // Retry/edit: the turn row exists (inserted by the mutation commit); fill the preflight columns.
            turn::Entity::update_many()
                .col_expr(
                    turn::Column::ReserveTokens,
                    Expr::value(Some(d.reserve_tokens)),
                )
                .col_expr(
                    turn::Column::MaxOutputTokensApplied,
                    Expr::value(Some(
                        i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX),
                    )),
                )
                .col_expr(
                    turn::Column::ReservedCreditsMicro,
                    Expr::value(Some(d.reserved_credits_micro)),
                )
                .col_expr(
                    turn::Column::PolicyVersionApplied,
                    Expr::value(Some(
                        i64::try_from(d.snapshot.policy_version).unwrap_or(i64::MAX),
                    )),
                )
                .col_expr(
                    turn::Column::EffectiveModel,
                    Expr::value(Some(d.effective.id.clone())),
                )
                .col_expr(
                    turn::Column::MinimalGenerationFloorApplied,
                    Expr::value(Some(i32::try_from(d.floor_applied).unwrap_or(i32::MAX))),
                )
                .col_expr(turn::Column::UpdatedAt, Expr::value(ts))
                .filter(
                    Condition::all()
                        .add(turn::Column::Id.eq(r.turn_id))
                        .add(turn::Column::State.eq("running")),
                )
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?;
        }
        Ok(ts)
    }

    /// Spawns the provider task and returns the live stream (with `stream_started` queued).
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "turn identity fields produced by the caller's reserve transaction plus the prepared request"
    )]
    pub fn spawn_turn(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat: &chat::Model,
        turn_id: Uuid,
        request_id: Uuid,
        user_msg_id: Uuid,
        user_msg_created: OffsetDateTime,
        prepared: PreparedTurn,
    ) -> LiveStream {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel::<SseEvent>(cap);
        let cancel = CancellationToken::new();
        let assistant_message_id = Uuid::new_v4();
        let mut started = json!({"request_id": request_id, "message_id": assistant_message_id, "is_new_turn": true});
        if let Some(t) = prepared.summary_token_estimate {
            started["thread_summary_applied"] = json!({"token_estimate": t.max(0)});
        }
        if let Err(e) = tx.try_send(SseEvent::new("stream_started", started)) {
            tracing::debug!(error = %e, turn_id = %turn_id, "mini-chat: could not queue stream_started");
        }
        let meta = TurnMeta {
            turn_id,
            chat_id: chat.id,
            tenant_id: chat.tenant_id,
            user_id: ctx.subject_id(),
            request_id,
            assistant_message_id,
            user_message_id: user_msg_id,
            user_message_created_at: user_msg_created,
            selected_model: chat.model.clone(),
            effective_model: prepared.decision.effective.id.clone(),
            effective_tier: prepared.decision.tier(),
            decision: prepared.decision.decision,
            downgrade_reason: prepared.decision.downgrade_reason,
            reserve: Some(ReserveSpec {
                tenant_id: chat.tenant_id,
                user_id: ctx.subject_id(),
                tier: prepared.decision.tier(),
                reserved_credits_micro: prepared.decision.reserved_credits_micro,
                daily_start: prepared.decision.daily_start,
                monthly_start: prepared.decision.monthly_start,
            }),
            reserve_tokens: prepared.decision.reserve_tokens,
            max_output_tokens_applied: prepared.decision.max_output_tokens_applied,
            floor_applied: prepared.decision.floor_applied,
            policy_version: prepared.decision.snapshot.policy_version,
            assembled_tokens: prepared.plan.assembled_tokens,
            effective_budget: prepared.plan.effective_budget,
            messages_truncated: prepared.plan.messages_truncated,
            summary_exists: prepared.summary_exists,
            provider_id: prepared.provider.id.clone(),
        };
        let core = Arc::clone(self);
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            core.run_provider_task(ctx, meta, prepared, tx, task_cancel)
                .await;
        });
        LiveStream { rx, cancel }
    }

    /// Provider task: relays events, enforces tool limits, finalizes the turn.
    #[allow(
        clippy::cognitive_complexity,
        reason = "single select! event loop mapping provider events to SSE; splitting would fragment the loop state"
    )]
    async fn run_provider_task(
        self: Arc<Self>,
        ctx: SecurityContext,
        meta: TurnMeta,
        prepared: PreparedTurn,
        tx: mpsc::Sender<SseEvent>,
        cancel: CancellationToken,
    ) {
        let started = Instant::now();
        let (ptx, mut prx) = mpsc::channel::<LlmEvent>(64);
        let provider_task = tokio::spawn(responses::stream_chat(
            Arc::clone(&self.transport),
            ctx.clone(),
            prepared.provider.clone(),
            prepared.request.clone(),
            ptx,
        ));
        let ping_every =
            Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds));
        let mut ping_deadline = tokio::time::Instant::now() + ping_every;
        let mut content_started = false;
        let mut text = String::new();
        let mut counts = ToolCounters::default();
        let mut last_progress = Instant::now();
        let mut ttft: Option<u64> = None;
        let quota = self.cfg.quota.clone();

        let terminal: Terminal = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    provider_task.abort();
                    break Terminal::Cancelled;
                }
                () = tokio::time::sleep_until(ping_deadline), if !content_started => {
                    ping_deadline = tokio::time::Instant::now() + ping_every;
                    if tx.send(SseEvent::new("ping", json!({}))).await.is_err() {
                        provider_task.abort();
                        break Terminal::Cancelled;
                    }
                }
                ev = prx.recv() => {
                    let Some(ev) = ev else {
                        break Terminal::Failed {
                            code: "provider_error".to_owned(),
                            message: "Provider stream ended without a terminal event".to_owned(),
                            usage: None,
                        };
                    };
                    ping_deadline = tokio::time::Instant::now() + ping_every;
                    let out = match ev {
                        LlmEvent::Text(t) => {
                            if ttft.is_none() {
                                ttft = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                            }
                            text.push_str(&t);
                            content_started = true;
                            Some(SseEvent::new("delta", json!({"type": "text", "content": t})))
                        }
                        LlmEvent::Reasoning(t) => {
                            content_started = true;
                            Some(SseEvent::new("delta", json!({"type": "reasoning", "content": t})))
                        }
                        LlmEvent::ToolStart { name } => {
                            content_started = true;
                            match name {
                                "web_search" => counts.web_search_started += 1,
                                "code_interpreter" => counts.code_interpreter_started += 1,
                                _ => {}
                            }
                            if counts.web_search_started > quota.web_search_max_calls_per_message {
                                provider_task.abort();
                                break Terminal::Failed {
                                    code: "web_search_calls_exceeded".to_owned(),
                                    message: "Per-turn web search call limit exceeded".to_owned(),
                                    usage: None,
                                };
                            }
                            if counts.code_interpreter_started > quota.code_interpreter_max_calls_per_message {
                                provider_task.abort();
                                break Terminal::Failed {
                                    code: "code_interpreter_calls_exceeded".to_owned(),
                                    message: "Per-turn code interpreter call limit exceeded".to_owned(),
                                    usage: None,
                                };
                            }
                            Some(SseEvent::new("tool", json!({"phase": "start", "name": name, "details": {}})))
                        }
                        LlmEvent::ToolDone { name, details } => {
                            content_started = true;
                            match name {
                                "web_search" => counts.web_search_completed += 1,
                                "code_interpreter" => counts.code_interpreter_completed += 1,
                                "file_search" => counts.file_search_completed += 1,
                                _ => {}
                            }
                            Some(SseEvent::new("tool", json!({"phase": "done", "name": name, "details": details})))
                        }
                        LlmEvent::UnexpectedToolUse(name) => {
                            provider_task.abort();
                            break Terminal::Failed {
                                code: "unexpected_tool_use".to_owned(),
                                message: format!("The model requested an unsupported tool ({name})"),
                                usage: None,
                            };
                        }
                        LlmEvent::Completed(c) => {
                            if let Some(t) = &c.output_text
                                && text.is_empty()
                            {
                                text.push_str(t);
                                if tx.send(SseEvent::new("delta", json!({"type": "text", "content": t}))).await.is_err() {
                                    break Terminal::Cancelled;
                                }
                            }
                            break Terminal::Completed(c);
                        }
                        LlmEvent::Failed { kind, message, usage } => {
                            break Terminal::Failed { code: kind.code().to_owned(), message, usage };
                        }
                    };
                    if let Some(sse) = out {
                        if tx.send(sse).await.is_err() {
                            provider_task.abort();
                            break Terminal::Cancelled;
                        }
                        if last_progress.elapsed() >= Duration::from_secs(30) {
                            last_progress = Instant::now();
                            self.touch_progress(&meta, &counts).await;
                        }
                    }
                }
            }
        };
        provider_task.abort();
        let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let input = FinalizeInput {
            meta,
            terminal,
            text,
            counts,
            citation_map: prepared.attachments.citation_map,
            latency_ms: latency,
            ttft_ms: ttft,
        };
        let events = self.finalize_turn(input).await;
        for e in events {
            if tx.send(e).await.is_err() {
                break;
            }
        }
    }

    async fn touch_progress(&self, meta: &TurnMeta, counts: &ToolCounters) {
        let Ok(conn) = self.db.conn() else { return };
        if let Err(e) = turn::Entity::update_many()
            .col_expr(turn::Column::LastProgressAt, Expr::value(Some(now())))
            .col_expr(
                turn::Column::WebSearchCompletedCount,
                Expr::value(counts.web_search_completed),
            )
            .col_expr(
                turn::Column::CodeInterpreterCompletedCount,
                Expr::value(counts.code_interpreter_completed),
            )
            .col_expr(
                turn::Column::FileSearchCompletedCount,
                Expr::value(counts.file_search_completed),
            )
            .filter(
                Condition::all()
                    .add(turn::Column::Id.eq(meta.turn_id))
                    .add(turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&tenant_scope(meta.tenant_id))
            .exec(&conn)
            .await
        {
            tracing::debug!(error = %e, turn_id = %meta.turn_id, "mini-chat: progress heartbeat update failed");
        }
    }

    /// Replay events of a completed turn (pure read; no quota, outbox or provider call).
    ///
    /// # Errors
    /// Internal error when the stored assistant message is missing.
    pub async fn replay_events(
        &self,
        chat: &chat::Model,
        t: &turn::Model,
    ) -> Result<Vec<SseEvent>, DomainError> {
        let conn = self.db.conn()?;
        let msg_id = t
            .assistant_message_id
            .ok_or_else(|| DomainError::internal("completed turn without assistant message"))?;
        let m = message::Entity::find()
            .filter(message::Column::Id.eq(msg_id))
            .secure()
            .scope_with(&tenant_scope(chat.tenant_id))
            .one(&conn)
            .await?
            .ok_or_else(|| {
                DomainError::internal("assistant message of completed turn not found")
            })?;
        let effective = m
            .model
            .clone()
            .or_else(|| t.effective_model.clone())
            .unwrap_or_else(|| chat.model.clone());
        let downgrade = effective != chat.model;
        let mut done = json!({
            "usage": {"input_tokens": m.input_tokens, "output_tokens": m.output_tokens},
            "effective_model": effective,
            "selected_model": chat.model,
            "quota_decision": if downgrade { "downgrade" } else { "allow" },
        });
        if downgrade {
            done["downgrade_from"] = json!(chat.model);
        }
        Ok(vec![
            SseEvent::new(
                "stream_started",
                json!({"request_id": t.request_id, "message_id": msg_id, "is_new_turn": false}),
            ),
            SseEvent::new("delta", json!({"type": "text", "content": m.content})),
            SseEvent::new("done", done),
        ])
    }
}

/// Per-turn tool counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounters {
    pub web_search_started: u32,
    pub code_interpreter_started: u32,
    pub web_search_completed: i32,
    pub code_interpreter_completed: i32,
    pub file_search_completed: i32,
}

/// Mutation context of a reserve transaction (retry/edit fill the existing turn).
#[derive(Debug, Clone, Copy)]
pub struct MutationMarker;

/// Inputs of the reserve transaction.
#[derive(Debug, Clone)]
pub struct ReserveTx {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub user_msg_id: Uuid,
    pub content: String,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
    pub decision: PreflightDecision,
    pub mutation: Option<MutationMarker>,
}

#[must_use]
pub fn request_id_conflict() -> DomainError {
    DomainError::aborted(
        Resource::Turn,
        "request_id_conflict",
        "The request_id is already used by another turn",
    )
}

#[must_use]
pub fn turn_already_running() -> DomainError {
    DomainError::aborted(
        Resource::Turn,
        "turn_already_running",
        "A response is already being generated for this chat",
    )
}

/// Finds a turn by `(chat_id, request_id)` including soft-deleted ones.
///
/// # Errors
/// DB errors.
pub async fn find_turn(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<turn::Model>, DomainError> {
    Ok(turn::Entity::find()
        .filter(
            Condition::all()
                .add(turn::Column::ChatId.eq(chat_id))
                .add(turn::Column::RequestId.eq(request_id)),
        )
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .one(db)
        .await?)
}

/// `true` when a non-deleted running turn exists in the chat.
///
/// # Errors
/// DB errors.
pub async fn has_running_turn(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<bool, DomainError> {
    let n = turn::Entity::find()
        .filter(
            Condition::all()
                .add(turn::Column::ChatId.eq(chat_id))
                .add(turn::Column::State.eq("running"))
                .add(turn::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .count(db)
        .await?;
    Ok(n > 0)
}

/// Loads the referenced attachments of the chat (non-deleted).
///
/// # Errors
/// DB errors.
pub async fn load_referenced_attachments(
    db: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<attachment::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(attachment::Entity::find()
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(chat_id))
                .add(attachment::Column::Id.is_in(ids.iter().copied()))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .all(db)
        .await?)
}

/// Inserts a user message.
///
/// # Errors
/// DB errors.
pub async fn insert_user_message(
    tx: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    request_id: Uuid,
    content: &str,
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = message::ActiveModel {
        id: ActiveValue::Set(id),
        tenant_id: ActiveValue::Set(tenant_id),
        chat_id: ActiveValue::Set(chat_id),
        request_id: ActiveValue::Set(Some(request_id)),
        role: ActiveValue::Set("user".to_owned()),
        content: ActiveValue::Set(content.to_owned()),
        content_type: ActiveValue::Set("text".to_owned()),
        token_estimate: ActiveValue::Set(0),
        provider_response_id: ActiveValue::Set(None),
        request_kind: ActiveValue::Set("chat".to_owned()),
        features_used: ActiveValue::Set(json!([])),
        input_tokens: ActiveValue::Set(0),
        output_tokens: ActiveValue::Set(0),
        cache_read_input_tokens: ActiveValue::Set(0),
        cache_write_input_tokens: ActiveValue::Set(0),
        reasoning_tokens: ActiveValue::Set(0),
        model: ActiveValue::Set(None),
        is_compressed: ActiveValue::Set(false),
        created_at: ActiveValue::Set(ts),
        deleted_at: ActiveValue::Set(None),
    };
    message::Entity::insert(am)
        .secure()
        .scope_unchecked(&tenant_scope(tenant_id))?
        .exec(tx)
        .await?;
    Ok(())
}

/// Validates `attachment_ids` and inserts the `message_attachments` rows.
///
/// # Errors
/// 400 `invalid_attachment`.
pub async fn link_attachments(
    tx: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    if ids.is_empty() {
        return Ok(());
    }
    let rows = load_referenced_attachments(tx, tenant_id, chat_id, ids).await?;
    for id in ids {
        let ok = rows.iter().any(|a| {
            a.id == *id
                && a.tenant_id == tenant_id
                && a.uploaded_by_user_id == user_id
                && a.chat_id == chat_id
                && a.status == "ready"
        });
        if !ok {
            return Err(invalid_attachment(
                "attachment is unknown, foreign or not ready",
            ));
        }
        insert_link(tx, tenant_id, chat_id, message_id, *id, ts).await?;
    }
    Ok(())
}

/// Inserts one `message_attachments` row.
///
/// # Errors
/// DB errors.
pub async fn insert_link(
    tx: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = message_attachment::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        chat_id: ActiveValue::Set(chat_id),
        message_id: ActiveValue::Set(message_id),
        attachment_id: ActiveValue::Set(attachment_id),
        created_at: ActiveValue::Set(ts),
    };
    message_attachment::Entity::insert(am)
        .secure()
        .scope_unchecked(&tenant_scope(tenant_id))?
        .exec(tx)
        .await?;
    Ok(())
}

/// Inserts a running turn (preflight columns set when `decision` is given).
///
/// # Errors
/// DB errors (unique violations surface as internal errors containing "UNIQUE").
pub async fn insert_running_turn(
    tx: &impl DBRunner,
    r: &ReserveTx,
    decision: Option<&PreflightDecision>,
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = turn::ActiveModel {
        id: ActiveValue::Set(r.turn_id),
        tenant_id: ActiveValue::Set(r.tenant_id),
        chat_id: ActiveValue::Set(r.chat_id),
        request_id: ActiveValue::Set(r.request_id),
        requester_type: ActiveValue::Set("user".to_owned()),
        requester_user_id: ActiveValue::Set(Some(r.user_id)),
        state: ActiveValue::Set("running".to_owned()),
        provider_name: ActiveValue::Set(None),
        provider_response_id: ActiveValue::Set(None),
        assistant_message_id: ActiveValue::Set(None),
        error_code: ActiveValue::Set(None),
        reserve_tokens: ActiveValue::Set(decision.map(|d| d.reserve_tokens)),
        max_output_tokens_applied: ActiveValue::Set(
            decision.map(|d| i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX)),
        ),
        reserved_credits_micro: ActiveValue::Set(decision.map(|d| d.reserved_credits_micro)),
        policy_version_applied: ActiveValue::Set(
            decision.map(|d| i64::try_from(d.snapshot.policy_version).unwrap_or(i64::MAX)),
        ),
        effective_model: ActiveValue::Set(decision.map(|d| d.effective.id.clone())),
        minimal_generation_floor_applied: ActiveValue::Set(
            decision.map(|d| i32::try_from(d.floor_applied).unwrap_or(i32::MAX)),
        ),
        error_detail: ActiveValue::Set(None),
        deleted_at: ActiveValue::Set(None),
        replaced_by_request_id: ActiveValue::Set(None),
        started_at: ActiveValue::Set(ts),
        last_progress_at: ActiveValue::Set(Some(ts)),
        web_search_enabled: ActiveValue::Set(r.web_search),
        web_search_completed_count: ActiveValue::Set(0),
        code_interpreter_completed_count: ActiveValue::Set(0),
        file_search_completed_count: ActiveValue::Set(0),
        completed_at: ActiveValue::Set(None),
        updated_at: ActiveValue::Set(ts),
    };
    turn::Entity::insert(am)
        .secure()
        .scope_unchecked(&tenant_scope(r.tenant_id))?
        .exec(tx)
        .await?;
    Ok(())
}

impl From<QuotaDecision> for &'static str {
    fn from(d: QuotaDecision) -> Self {
        d.as_str()
    }
}
