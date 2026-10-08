//! Streaming turn pipeline (DESIGN §3.3 streaming contract, §3.6 sequence).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot, UserLimits};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::DbTx;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::app::{App, now, owner_scope, tenant_scope};
use super::authz::{actions, chat_scope};
use super::chats::{load_chat, touch_chat};
use super::context::{self, ContextPlan, HistoryMessage};
use super::error::DomainError;
use super::finalize::{Finalized, Outcome, ToolCounts, TurnRecord};
use super::quota::{self, CascadeDecision, PeriodStarts, PreflightInput, TurnReserve};
use super::sanitize::sanitize;
use crate::infra::db::entity::{attachments, chat_turns, chat_vector_stores, chats, message_attachments, messages, thread_summaries};
use crate::infra::llm::client::stream_chat;
use crate::infra::llm::{ChatRequest, FileSearchTool, ProviderEvent, RawCitation, RequestTools, ResolvedProvider, provider_user};

/// Interval of durable progress updates of a running turn.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Send-message input.
#[derive(Debug, Clone, Default)]
pub struct SendInput {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// One citation of the `citations` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationOut {
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(usize, usize)>,
}

/// One quota warning of the `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarningOut {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: i64,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<OffsetDateTime>,
}

/// `done` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneOut {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<WarningOut>>,
}

/// Events after `stream_started`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Delta { kind: &'static str, content: String },
    Tool { phase: &'static str, name: String, details: Value },
    Citations(Vec<CitationOut>),
    Done(DoneOut),
    Error { code: String, message: String },
}

impl StreamEvent {
    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }
}

/// `stream_started` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamHeader {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    pub summary_token_estimate: Option<i64>,
}

/// A live generation.
pub struct LiveStream {
    pub header: StreamHeader,
    pub rx: mpsc::Receiver<StreamEvent>,
    pub cancel: CancellationToken,
}

/// Outcome of stream setup.
pub enum StreamStart {
    Replay { header: StreamHeader, events: Vec<StreamEvent> },
    Live(LiveStream),
}

/// Facts about the chat used by preflight and tool selection.
#[derive(Debug, Clone, Default)]
pub struct ChatFacts {
    pub has_ready_documents: bool,
    pub ci_file_ids: Vec<String>,
    pub vector_store_id: Option<String>,
    pub prior_context_tokens: i64,
    /// provider file id -> (attachment id, filename) of ready attachments.
    pub file_map: HashMap<String, (Uuid, String)>,
}

/// Images referenced by a message.
#[derive(Debug, Clone, Default)]
pub struct ReferencedImages {
    pub count: u32,
    pub provider_file_ids: Vec<String>,
}

/// Preflight result (decision before context assembly).
pub struct Preflight {
    pub snapshot: PolicySnapshot,
    pub limits: UserLimits,
    pub decision: CascadeDecision,
    pub input: PreflightInput,
}

/// Everything needed to call the provider once the turn is committed.
pub struct PreparedCall {
    pub plan: ContextPlan,
    pub provider: ResolvedProvider,
    pub request: ChatRequest,
    pub has_summary: bool,
}

/// Loads chat facts (ready attachments, vector store, prior context tokens).
///
/// # Errors
/// Database errors.
pub async fn load_chat_facts(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<ChatFacts, DomainError> {
    let scope = tenant_scope(tenant_id);
    let ready = attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::Status.eq("ready")),
        )
        .all(runner)
        .await?;
    let mut facts = ChatFacts::default();
    for a in &ready {
        if a.for_file_search {
            facts.has_ready_documents = true;
        }
        if let Some(pf) = &a.provider_file_id {
            if a.for_code_interpreter {
                facts.ci_file_ids.push(pf.clone());
            }
            facts.file_map.insert(pf.clone(), (a.id, a.filename.clone()));
        }
    }
    facts.vector_store_id = chat_vector_stores::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(chat_id)))
        .one(runner)
        .await?
        .and_then(|v| v.vector_store_id);
    let prior = messages::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(messages::Column::Role.eq("assistant"))
                .add(
                    Condition::any()
                        .add(messages::Column::InputTokens.gt(0))
                        .add(messages::Column::OutputTokens.gt(0)),
                ),
        )
        .order_by(messages::Column::CreatedAt, Order::Desc)
        .order_by(messages::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    facts.prior_context_tokens = prior.map_or(0, |m| m.input_tokens + m.output_tokens);
    Ok(facts)
}

/// Loads the image attachments among `ids` (chat-scoped, non-deleted).
///
/// # Errors
/// Database errors.
pub async fn load_images(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid, ids: &[Uuid]) -> Result<ReferencedImages, DomainError> {
    if ids.is_empty() {
        return Ok(ReferencedImages::default());
    }
    let rows = attachments::Entity::find()
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::Id.is_in(ids.to_vec()))
                .add(attachments::Column::AttachmentKind.eq("image")),
        )
        .all(runner)
        .await?;
    let mut out = ReferencedImages::default();
    for id in ids {
        if let Some(r) = rows.iter().find(|r| r.id == *id) {
            out.count += 1;
            if let Some(pf) = &r.provider_file_id {
                out.provider_file_ids.push(pf.clone());
            }
        }
    }
    Ok(out)
}

/// Validates referenced attachments inside the reserve transaction and links
/// them to the user message.
///
/// # Errors
/// `InvalidAttachment` or database errors.
pub async fn link_attachments(
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    require_ready: bool,
    at: OffsetDateTime,
) -> Result<(), DomainError> {
    if ids.is_empty() {
        return Ok(());
    }
    let scope = tenant_scope(tenant_id);
    let rows = attachments::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(attachments::Column::TenantId.eq(tenant_id))
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::Id.is_in(ids.to_vec())),
        )
        .all(tx)
        .await?;
    for id in ids {
        let Some(a) = rows.iter().find(|a| a.id == *id) else {
            return Err(DomainError::InvalidAttachment(format!("attachment {id} is not available in this chat")));
        };
        if a.uploaded_by_user_id != user_id {
            return Err(DomainError::InvalidAttachment(format!("attachment {id} is not available in this chat")));
        }
        if require_ready && a.status != "ready" {
            return Err(DomainError::InvalidAttachment(format!("attachment {id} is not ready")));
        }
        let am = message_attachments::ActiveModel {
            tenant_id: ActiveValue::Set(tenant_id),
            chat_id: ActiveValue::Set(chat_id),
            message_id: ActiveValue::Set(message_id),
            attachment_id: ActiveValue::Set(*id),
            created_at: ActiveValue::Set(at),
        };
        message_attachments::Entity::insert(am).secure().scope_unchecked(&scope)?.exec(tx).await?;
    }
    Ok(())
}

/// Validates the attachment id list shape (duplicates, length).
///
/// # Errors
/// `InvalidAttachment`.
pub fn validate_attachment_ids(ids: &[Uuid], max: u32) -> Result<(), DomainError> {
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Err(DomainError::InvalidAttachment("duplicate attachment id".into()));
        }
    }
    if ids.len() > usize::try_from(max).unwrap_or(usize::MAX) {
        return Err(DomainError::InvalidAttachment("too many attachment ids".into()));
    }
    Ok(())
}

/// Inserts a user message.
///
/// # Errors
/// Database errors.
pub async fn insert_user_message(
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    request_id: Uuid,
    content: &str,
    at: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = messages::ActiveModel {
        id: ActiveValue::Set(message_id),
        tenant_id: ActiveValue::Set(tenant_id),
        chat_id: ActiveValue::Set(chat_id),
        request_id: ActiveValue::Set(Some(request_id)),
        role: ActiveValue::Set("user".into()),
        content: ActiveValue::Set(content.to_owned()),
        content_type: ActiveValue::Set("text".into()),
        token_estimate: ActiveValue::Set(0),
        provider_response_id: ActiveValue::Set(None),
        request_kind: ActiveValue::Set("chat".into()),
        features_used: ActiveValue::Set(serde_json::json!([])),
        input_tokens: ActiveValue::Set(0),
        output_tokens: ActiveValue::Set(0),
        cache_read_input_tokens: ActiveValue::Set(0),
        cache_write_input_tokens: ActiveValue::Set(0),
        reasoning_tokens: ActiveValue::Set(0),
        model: ActiveValue::Set(None),
        is_compressed: ActiveValue::Set(false),
        created_at: ActiveValue::Set(at),
        deleted_at: ActiveValue::Set(None),
    };
    messages::Entity::insert(am).secure().scope_unchecked(&tenant_scope(tenant_id))?.exec(tx).await?;
    Ok(())
}

/// Finds a turn by `(chat_id, request_id)` including soft-deleted turns.
///
/// # Errors
/// Database errors.
pub async fn find_turn(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid, request_id: Uuid) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .one(runner)
        .await?)
}

/// Whether the chat has a running turn.
///
/// # Errors
/// Database errors.
pub async fn running_turn(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::State.eq("running"))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?)
}

fn web_snippet(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    match (start, end) {
        (Some(s), Some(e)) if s < e => text.chars().skip(s).take(e - s).collect(),
        _ => String::new(),
    }
}

/// Maps raw provider citations to client citations.
#[must_use]
pub fn map_citations(raw: &[RawCitation], text: &str, file_map: &HashMap<String, (Uuid, String)>) -> Vec<CitationOut> {
    let mut out = Vec::new();
    for c in raw {
        match c {
            RawCitation::Url { url, title, start, end } => out.push(CitationOut {
                source: "web",
                title: title.clone(),
                url: Some(url.clone()),
                attachment_id: None,
                snippet: web_snippet(text, *start, *end),
                span: match (start, end) {
                    (Some(s), Some(e)) => Some((*s, *e)),
                    _ => None,
                },
            }),
            RawCitation::File { file_id, .. } => {
                if let Some((id, filename)) = file_map.get(file_id)
                    && !out.iter().any(|o: &CitationOut| o.attachment_id == Some(*id))
                {
                    out.push(CitationOut {
                        source: "file",
                        title: filename.clone(),
                        url: None,
                        attachment_id: Some(*id),
                        snippet: String::new(),
                        span: None,
                    });
                }
            }
        }
    }
    out
}

/// Data of a running generation handed to the provider task.
pub struct TurnRun {
    pub rec: TurnRecord,
    pub provider: ResolvedProvider,
    pub request: ChatRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub summary_trigger: bool,
}

impl App {
    /// Runs the shared preflight: chat model, image count, web search kill
    /// switch, quota cascade, tool quotas, input limit and image guards.
    ///
    /// # Errors
    /// The JSON error of the first failed check.
    #[allow(clippy::too_many_arguments)]
    pub async fn preflight(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        chat: &chats::Model,
        facts: &ChatFacts,
        content: &str,
        images: &ReferencedImages,
        web_search: bool,
    ) -> Result<Preflight, DomainError> {
        let snapshot = self.policy.current_snapshot(user_id).await?;
        if snapshot.find(&chat.model).is_none() {
            return Err(DomainError::InvalidModel);
        }
        if images.count > self.cfg.rag.max_images_per_message {
            return Err(DomainError::TooManyImages);
        }
        if web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        let limits = self.policy.user_limits(user_id, snapshot.policy_version).await?;
        let usage = {
            let conn = self.db.conn()?;
            quota::load_usage(&conn, &owner_scope(tenant_id, user_id), tenant_id, user_id, PeriodStarts::at(now())).await?
        };
        let input = PreflightInput {
            content_bytes: content.len(),
            image_count: images.count,
            prior_context_tokens: facts.prior_context_tokens,
            chat_has_ready_documents: facts.has_ready_documents,
            chat_has_ready_ci_files: !facts.ci_file_ids.is_empty(),
            web_search_requested: web_search,
            max_output_cap: self.cfg.streaming.max_output_tokens,
            minimal_generation_floor: self.cfg.estimation_budgets.minimal_generation_floor,
        };
        let decision = quota::resolve_effective_model(&snapshot, &chat.model, &usage, &limits, &input)?;
        quota::check_tool_quotas(
            decision.tools,
            &usage,
            self.cfg.quota.web_search_daily_quota,
            self.cfg.quota.code_interpreter_daily_quota,
        )?;
        let eff = &decision.effective;
        if eff.max_input_tokens > 0
            && quota::estimate_text_tokens(content.len(), &eff.estimation_budgets) > i64::from(eff.max_input_tokens)
        {
            return Err(DomainError::InputTooLong);
        }
        if images.count > 0 && snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        if images.count > 0 && !eff.supports_vision() {
            return Err(DomainError::VisionNotSupported);
        }
        Ok(Preflight { snapshot, limits, decision, input })
    }

    /// Assembles the context and the provider request for the effective model.
    ///
    /// # Errors
    /// `ContextBudgetExceeded`, provider resolution (internal) or database errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare_call(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
        pre: &Preflight,
        facts: &ChatFacts,
        content: &str,
        images: &ReferencedImages,
        exclude_request_id: Option<Uuid>,
    ) -> Result<PreparedCall, DomainError> {
        let eff: &ModelCatalogEntry = &pre.decision.effective;
        let tools = quota::ToolSelection {
            file_search: pre.decision.tools.file_search && facts.vector_store_id.is_some(),
            ..pre.decision.tools
        };
        let (summary, history) = {
            let conn = self.db.conn()?;
            self.load_history(&conn, tenant_id, chat_id, exclude_request_id).await?
        };
        let plan = context::assemble(&context::ContextInput {
            model: eff,
            max_output_tokens_applied: pre.decision.reserve.max_output_tokens_applied,
            tools,
            web_search_guard: &self.cfg.context.web_search_guard,
            file_search_guard: &self.cfg.context.file_search_guard,
            summary: summary.as_ref().map(|s| (s.summary_text.as_str(), i64::from(s.token_estimate))),
            recent: &history,
            user_message: content,
            image_file_ids: &images.provider_file_ids,
        })?;
        let provider = self
            .resolver
            .resolve(&eff.provider_id, tenant_id)
            .ok_or_else(|| DomainError::internal(format!("provider '{}' is not configured", eff.provider_id)))?;
        let mut req_tools = RequestTools::default();
        if tools.file_search
            && let Some(vs) = &facts.vector_store_id
        {
            req_tools.file_search = Some(FileSearchTool {
                vector_store_id: vs.clone(),
                max_num_results: eff.max_num_results,
            });
        }
        if tools.web_search {
            req_tools.web_search = Some(eff.web_search_context_size.as_str().to_owned());
        }
        if tools.code_interpreter && !facts.ci_file_ids.is_empty() {
            req_tools.code_interpreter = Some(facts.ci_file_ids.clone());
        }
        if !req_tools.is_empty() {
            req_tools.max_tool_calls = Some(eff.max_tool_calls);
        }
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), Value::String(tenant_id.to_string()));
        metadata.insert("user_id".into(), Value::String(user_id.to_string()));
        metadata.insert("chat_id".into(), Value::String(chat_id.to_string()));
        metadata.insert("request_type".into(), Value::String("chat".into()));
        metadata.insert("feature".into(), Value::String(req_tools.feature()));
        let request = ChatRequest {
            model: eff.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input: plan.input.clone(),
            tools: req_tools,
            max_output_tokens: u32::try_from(pre.decision.reserve.max_output_tokens_applied).unwrap_or(u32::MAX),
            user: provider_user(tenant_id, user_id),
            metadata,
            api_params: eff.general_config.api_params.clone(),
            stream: true,
        };
        Ok(PreparedCall { has_summary: summary.is_some(), plan, provider, request })
    }

    /// Loads the thread summary and the recent uncompressed messages
    /// (chronological), excluding a request id (the current turn).
    async fn load_history(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        exclude_request_id: Option<Uuid>,
    ) -> Result<(Option<thread_summaries::Model>, Vec<HistoryMessage>), DomainError> {
        let scope = tenant_scope(tenant_id);
        let summary = thread_summaries::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
            .one(runner)
            .await?;
        let mut cond = Condition::all()
            .add(messages::Column::ChatId.eq(chat_id))
            .add(messages::Column::RequestId.is_not_null())
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false));
        if let Some(rid) = exclude_request_id {
            cond = cond.add(messages::Column::RequestId.ne(rid));
        }
        if let Some(s) = &summary {
            cond = cond.add(
                Condition::any()
                    .add(messages::Column::CreatedAt.gt(s.summarized_up_to_created_at))
                    .add(
                        Condition::all()
                            .add(messages::Column::CreatedAt.eq(s.summarized_up_to_created_at))
                            .add(messages::Column::Id.gt(s.summarized_up_to_message_id)),
                    ),
            );
        }
        let limit = u64::from(self.cfg.context.recent_messages_limit);
        let mut rows = if limit == 0 {
            Vec::new()
        } else {
            messages::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(cond)
                .order_by(messages::Column::CreatedAt, Order::Desc)
                .order_by(messages::Column::Id, Order::Desc)
                .limit(limit)
                .all(runner)
                .await?
        };
        rows.reverse();
        let history = rows
            .into_iter()
            .filter(|m| m.role != "system")
            .map(|m| HistoryMessage { role: m.role, content: m.content })
            .collect();
        Ok((summary, history))
    }

    /// `POST /v1/chats/{id}/messages:stream` setup.
    ///
    /// # Errors
    /// Pre-stream JSON errors (validation, authz, conflicts, preflight).
    #[allow(clippy::too_many_lines)]
    pub async fn start_send(self: &Arc<Self>, ctx: SecurityContext, chat_id: Uuid, input: SendInput) -> Result<StreamStart, DomainError> {
        if input.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let scope = chat_scope(&self.enforcer, &ctx, actions::SEND_MESSAGE, Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        let request_id = input.request_id.unwrap_or_else(Uuid::new_v4);

        // 1. Idempotency (before the parallel-turn guard).
        if let Some(t) = find_turn(&conn, chat.tenant_id, chat.id, request_id).await? {
            if t.state == "completed" && t.deleted_at.is_none() {
                return self.replay(&conn, &chat, &t).await;
            }
            return Err(DomainError::RequestIdConflict);
        }
        // 2. Parallel turn guard.
        if running_turn(&conn, chat.tenant_id, chat.id).await?.is_some() {
            return Err(DomainError::TurnAlreadyRunning);
        }
        validate_attachment_ids(
            &input.attachment_ids,
            self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message,
        )?;
        let images = load_images(&conn, chat.tenant_id, chat.id, &input.attachment_ids).await?;
        let facts = load_chat_facts(&conn, chat.tenant_id, chat.id).await?;
        drop(conn);
        let pre = self
            .preflight(tenant_id, user_id, &chat, &facts, &input.content, &images, input.web_search)
            .await?;
        let call = self
            .prepare_call(tenant_id, user_id, chat.id, &pre, &facts, &input.content, &images, None)
            .await?;

        // 3. Reserve transaction.
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let assistant_message_id = Uuid::new_v4();
        let at = now();
        let reserve = pre.decision.reserve;
        let tier = pre.decision.effective.tier;
        let limits = pre.limits.clone();
        let policy_version = pre.snapshot.policy_version;
        let effective_model = pre.decision.effective.id.clone();
        let content = input.content.clone();
        let attachment_ids = input.attachment_ids.clone();
        let web_search = input.web_search;
        let chat_tenant = chat.tenant_id;
        let res = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    quota::write_reserve(
                        tx,
                        &owner_scope(tenant_id, user_id),
                        tenant_id,
                        user_id,
                        tier,
                        reserve.reserved_credits_micro,
                        PeriodStarts::at(at),
                        &limits,
                        at,
                    )
                    .await?;
                    insert_user_message(tx, chat_tenant, chat_id, user_message_id, request_id, &content, at).await?;
                    touch_chat(tx, chat_tenant, chat_id, at).await?;
                    link_attachments(tx, chat_tenant, user_id, chat_id, user_message_id, &attachment_ids, true, at).await?;
                    let am = chat_turns::ActiveModel {
                        id: ActiveValue::Set(turn_id),
                        tenant_id: ActiveValue::Set(chat_tenant),
                        chat_id: ActiveValue::Set(chat_id),
                        request_id: ActiveValue::Set(request_id),
                        requester_type: ActiveValue::Set("user".into()),
                        requester_user_id: ActiveValue::Set(Some(user_id)),
                        state: ActiveValue::Set("running".into()),
                        provider_name: ActiveValue::Set(None),
                        provider_response_id: ActiveValue::Set(None),
                        assistant_message_id: ActiveValue::Set(None),
                        error_code: ActiveValue::Set(None),
                        reserve_tokens: ActiveValue::Set(Some(reserve.reserve_tokens)),
                        max_output_tokens_applied: ActiveValue::Set(Some(
                            i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX),
                        )),
                        reserved_credits_micro: ActiveValue::Set(Some(reserve.reserved_credits_micro)),
                        policy_version_applied: ActiveValue::Set(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))),
                        effective_model: ActiveValue::Set(Some(effective_model)),
                        minimal_generation_floor_applied: ActiveValue::Set(Some(
                            i32::try_from(reserve.minimal_generation_floor_applied).unwrap_or(i32::MAX),
                        )),
                        error_detail: ActiveValue::Set(None),
                        deleted_at: ActiveValue::Set(None),
                        replaced_by_request_id: ActiveValue::Set(None),
                        started_at: ActiveValue::Set(at),
                        last_progress_at: ActiveValue::Set(Some(at)),
                        web_search_enabled: ActiveValue::Set(web_search),
                        web_search_completed_count: ActiveValue::Set(0),
                        code_interpreter_completed_count: ActiveValue::Set(0),
                        file_search_completed_count: ActiveValue::Set(0),
                        completed_at: ActiveValue::Set(None),
                        updated_at: ActiveValue::Set(at),
                    };
                    chat_turns::Entity::insert(am)
                        .secure()
                        .scope_unchecked(&tenant_scope(chat_tenant))?
                        .exec(tx)
                        .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = res {
            if matches!(e, DomainError::UniqueViolation) {
                let conn = self.db.conn()?;
                if find_turn(&conn, chat.tenant_id, chat.id, request_id).await?.is_some() {
                    return Err(DomainError::RequestIdConflict);
                }
                return Err(DomainError::TurnAlreadyRunning);
            }
            return Err(e);
        }
        let summary_trigger = self.cfg.thread_summary_worker.enabled
            && context::summary_trigger(&call.plan, call.has_summary, self.cfg.thread_summary_worker.compression_threshold_pct);
        let rec = TurnRecord {
            tenant_id: chat.tenant_id,
            user_id,
            chat_id: chat.id,
            turn_id,
            request_id,
            message_id: assistant_message_id,
            selected_model: chat.model.clone(),
            effective_model: pre.decision.effective.id.clone(),
            policy_version,
            reserve: TurnReserve {
                reserve_tokens: reserve.reserve_tokens,
                max_output_tokens_applied: reserve.max_output_tokens_applied,
                reserved_credits_micro: reserve.reserved_credits_micro,
                minimal_generation_floor_applied: reserve.minimal_generation_floor_applied,
            },
            started_at: at,
            downgrade_reason: pre.decision.downgrade_reason.map(str::to_owned),
        };
        let header = StreamHeader {
            request_id,
            message_id: assistant_message_id,
            is_new_turn: true,
            summary_token_estimate: call.plan.summary_applied,
        };
        Ok(StreamStart::Live(self.spawn_turn(
            header,
            TurnRun {
                rec,
                provider: call.provider,
                request: call.request,
                file_map: facts.file_map,
                summary_trigger,
            },
        )))
    }

    /// Replays a completed turn (no provider call, no quota or outbox change).
    ///
    /// # Errors
    /// Database errors or a completed turn without its message (internal).
    pub async fn replay(&self, runner: &impl DBRunner, chat: &chats::Model, turn: &chat_turns::Model) -> Result<StreamStart, DomainError> {
        let msg_id = turn
            .assistant_message_id
            .ok_or_else(|| DomainError::internal("completed turn without assistant message"))?;
        let msg = messages::Entity::find()
            .secure()
            .scope_with(&tenant_scope(chat.tenant_id))
            .filter(Condition::all().add(messages::Column::Id.eq(msg_id)))
            .one(runner)
            .await?
            .ok_or_else(|| DomainError::internal("assistant message of a completed turn is missing"))?;
        let effective = msg.model.clone().or_else(|| turn.effective_model.clone()).unwrap_or_default();
        let downgrade = effective != chat.model;
        let header = StreamHeader {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            summary_token_estimate: None,
        };
        let events = vec![
            StreamEvent::Delta { kind: "text", content: msg.content.clone() },
            StreamEvent::Done(DoneOut {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
                effective_model: effective,
                selected_model: chat.model.clone(),
                downgrade_from: downgrade.then(|| chat.model.clone()),
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ];
        Ok(StreamStart::Replay { header, events })
    }

    /// Spawns the provider task of a committed turn.
    pub fn spawn_turn(self: &Arc<Self>, header: StreamHeader, run: TurnRun) -> LiveStream {
        let capacity = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(capacity);
        let cancel = CancellationToken::new();
        let app = Arc::clone(self);
        let token = cancel.clone();
        tokio::spawn(async move {
            app.run_turn(run, tx, token).await;
        });
        LiveStream { header, rx, cancel }
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run_turn(self: Arc<Self>, run: TurnRun, tx: mpsc::Sender<StreamEvent>, cancel: CancellationToken) {
        let TurnRun { rec, provider, request, file_map, summary_trigger } = run;
        let mut text = String::new();
        let mut citations: Vec<RawCitation> = Vec::new();
        let mut response_id: Option<String> = None;
        let mut counts = ToolCounts::default();
        let mut web_started = 0_u32;
        let mut ci_started = 0_u32;
        let mut last_progress = Instant::now();
        let mut client_gone = false;
        let mut stream = tokio::select! {
            s = stream_chat(&self.transport, &provider, &request) => Some(s),
            () = cancel.cancelled() => None,
        };
        let outcome = loop {
            let Some(s) = stream.as_mut() else {
                client_gone = true;
                break Outcome::Cancelled { text: text.clone(), response_id: response_id.clone() };
            };
            let ev = tokio::select! {
                ev = s.next() => ev,
                () = cancel.cancelled() => {
                    client_gone = true;
                    break Outcome::Cancelled { text: text.clone(), response_id: response_id.clone() };
                }
            };
            let Some(ev) = ev else {
                break Outcome::Failed {
                    code: "provider_error".into(),
                    message: "Provider stream ended unexpectedly".into(),
                    usage: None,
                    response_id: response_id.clone(),
                };
            };
            let forward = match ev {
                ProviderEvent::ResponseId(id) => {
                    response_id = Some(id);
                    None
                }
                ProviderEvent::TextDelta(t) => {
                    text.push_str(&t);
                    Some(StreamEvent::Delta { kind: "text", content: t })
                }
                ProviderEvent::ReasoningDelta(t) => Some(StreamEvent::Delta { kind: "reasoning", content: t }),
                ProviderEvent::ToolStart { name, details } => {
                    if name == "web_search" {
                        web_started += 1;
                        if web_started > self.cfg.quota.web_search_max_calls_per_message {
                            break Outcome::Failed {
                                code: "web_search_calls_exceeded".into(),
                                message: "Web search call limit for this message exceeded".into(),
                                usage: None,
                                response_id: response_id.clone(),
                            };
                        }
                    }
                    if name == "code_interpreter" {
                        ci_started += 1;
                        if ci_started > self.cfg.quota.code_interpreter_max_calls_per_message {
                            break Outcome::Failed {
                                code: "code_interpreter_calls_exceeded".into(),
                                message: "Code interpreter call limit for this message exceeded".into(),
                                usage: None,
                                response_id: response_id.clone(),
                            };
                        }
                    }
                    Some(StreamEvent::Tool { phase: "start", name, details })
                }
                ProviderEvent::ToolDone { name, details } => {
                    match name.as_str() {
                        "web_search" => counts.web_search += 1,
                        "code_interpreter" => counts.code_interpreter += 1,
                        "file_search" => counts.file_search += 1,
                        _ => {}
                    }
                    Some(StreamEvent::Tool { phase: "done", name, details })
                }
                ProviderEvent::Citation(c) => {
                    citations.push(c);
                    None
                }
                ProviderEvent::Completed { response_id: rid, usage, incomplete_reason } => {
                    break Outcome::Completed {
                        text: text.clone(),
                        usage,
                        response_id: rid.or_else(|| response_id.clone()),
                        incomplete_reason,
                    };
                }
                ProviderEvent::Failed { kind, message, usage } => {
                    break Outcome::Failed {
                        code: kind.code().to_owned(),
                        message,
                        usage,
                        response_id: response_id.clone(),
                    };
                }
            };
            if let Some(ev) = forward {
                let is_progress = ev.is_content();
                if tx.send(ev).await.is_err() {
                    client_gone = true;
                    break Outcome::Cancelled { text: text.clone(), response_id: response_id.clone() };
                }
                if is_progress && last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    self.touch_progress(rec.tenant_id, rec.turn_id, counts).await;
                }
            }
        };
        // Dropping the provider stream aborts the outbound request.
        drop(stream);
        let outcome_kind = match &outcome {
            Outcome::Completed { .. } => "completed",
            Outcome::Failed { .. } => "failed",
            Outcome::Cancelled { .. } => "cancelled",
        };
        let failure = match &outcome {
            Outcome::Failed { code, message, .. } => Some((code.clone(), sanitize(message))),
            _ => None,
        };
        let completed_usage = match &outcome {
            Outcome::Completed { usage, .. } => usage.unwrap_or_default(),
            _ => crate::infra::llm::ProviderUsage::default(),
        };
        let result = self.finalize_turn(&rec, outcome, counts, summary_trigger).await;
        if client_gone || outcome_kind == "cancelled" {
            return;
        }
        let terminal = match result {
            Ok(Finalized::Committed { state: "completed", .. }) => {
                let mapped = map_citations(&citations, &text, &file_map);
                if !mapped.is_empty() && tx.send(StreamEvent::Citations(mapped)).await.is_err() {
                    return;
                }
                let warnings = self.quota_warnings(rec.tenant_id, rec.user_id).await;
                let downgrade = rec.effective_model != rec.selected_model || rec.downgrade_reason.is_some();
                Some(StreamEvent::Done(DoneOut {
                    input_tokens: completed_usage.input_tokens,
                    output_tokens: completed_usage.output_tokens,
                    effective_model: rec.effective_model.clone(),
                    selected_model: rec.selected_model.clone(),
                    downgrade_from: downgrade.then(|| rec.selected_model.clone()),
                    downgrade_reason: if downgrade { rec.downgrade_reason.clone() } else { None },
                    quota_warnings: Some(warnings),
                }))
            }
            Ok(Finalized::Committed { state: "failed", error_code: Some(code) }) if code == "message_persistence_failed" => {
                Some(StreamEvent::Error {
                    code,
                    message: "The response could not be saved".into(),
                })
            }
            Ok(Finalized::Committed { .. }) => failure.map(|(code, message)| StreamEvent::Error { code, message }),
            Ok(Finalized::CasLost) => None,
            Err(e) => {
                tracing::warn!(turn_id = %rec.turn_id, error = %e, "finalization failed");
                match failure {
                    Some((code, message)) => Some(StreamEvent::Error { code, message }),
                    None => Some(StreamEvent::Error {
                        code: "finalization_failed".into(),
                        message: "The response could not be finalized".into(),
                    }),
                }
            }
        };
        if let Some(t) = terminal {
            let _ = tx.send(t).await;
        }
    }

    async fn touch_progress(&self, tenant_id: Uuid, turn_id: Uuid, counts: ToolCounts) {
        let Ok(conn) = self.db.conn() else { return };
        let at = now();
        let res = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(Some(at)))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
            .col_expr(
                chat_turns::Column::WebSearchCompletedCount,
                Expr::value(i32::try_from(counts.web_search).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turns::Column::CodeInterpreterCompletedCount,
                Expr::value(i32::try_from(counts.code_interpreter).unwrap_or(i32::MAX)),
            )
            .col_expr(
                chat_turns::Column::FileSearchCompletedCount,
                Expr::value(i32::try_from(counts.file_search).unwrap_or(i32::MAX)),
            )
            .filter(
                Condition::all()
                    .add(chat_turns::Column::Id.eq(turn_id))
                    .add(chat_turns::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&tenant_scope(tenant_id))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "progress update failed");
        }
    }

    /// `done.quota_warnings` for the user (empty on failure).
    pub async fn quota_warnings(&self, tenant_id: Uuid, user_id: Uuid) -> Vec<WarningOut> {
        match self.quota_status_for(&owner_scope(tenant_id, user_id), tenant_id, user_id).await {
            Ok(list) => list
                .into_iter()
                .map(|p| WarningOut {
                    tier: p.tier,
                    period: p.period,
                    remaining_percentage: p.remaining_percentage,
                    warning: p.warning,
                    exhausted: p.exhausted,
                    next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "quota warnings unavailable");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_map_and_filter_unknown_files() {
        let id = Uuid::new_v4();
        let mut map = HashMap::new();
        map.insert("file-abc".to_owned(), (id, "Q3.pdf".to_owned()));
        let raw = vec![
            RawCitation::Url { url: "https://x".into(), title: "X".into(), start: Some(0), end: Some(5) },
            RawCitation::File { file_id: "file-abc".into(), filename: None },
            RawCitation::File { file_id: "file-unknown".into(), filename: Some("leak.pdf".into()) },
        ];
        let out = map_citations(&raw, "Hello world", &map);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].snippet, "Hello");
        assert_eq!(out[0].span, Some((0, 5)));
        assert_eq!(out[1].attachment_id, Some(id));
        assert_eq!(out[1].title, "Q3.pdf");
        assert_eq!(out[1].snippet, "");
    }

    #[test]
    fn attachment_id_validation() {
        let a = Uuid::new_v4();
        assert!(validate_attachment_ids(&[a, a], 10).is_err());
        assert!(validate_attachment_ids(&[a, Uuid::new_v4()], 1).is_err());
        assert!(validate_attachment_ids(&[a], 1).is_ok());
    }
}
