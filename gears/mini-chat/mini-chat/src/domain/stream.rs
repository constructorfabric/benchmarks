//! Stream service: `messages:stream` setup, idempotent replay and the
//! provider relay task (DESIGN §3.6 "Send Message with Streaming Response").

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::authz::actions;
use super::billing::{TurnState, codes};
use super::clock;
use super::context::{ContextInput, HistoryMessage, plan as plan_context};
use super::credits::{Surcharges, estimate_text_tokens};
use super::error::DomainError;
use super::finalize::{FinalizeResult, TerminalOutcome};
use super::policy::PolicyView;
use super::quota::{PreflightDecision, QuotaInputs, QuotaService};
use super::service::{ChatAccess, MiniChat};
use super::stream_types::{
    CancelOnDrop, Citation, CitationsData, DeltaData, DoneData, LiveStream, StreamEvent, StreamStart,
    StreamStartedData, TextSpan, ThreadSummaryInfo, ToolData, UsageData,
};
use crate::config::ProviderKind;
use crate::infra::llm::types::{
    LlmMessage, LlmMetadata, LlmPart, LlmRequest, LlmRole, LlmTool, ProviderEvent, ProviderUsage, RawCitation,
};
use crate::infra::llm::ResolvedProvider;
use crate::infra::storage::entity::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, thread_summary,
};

/// Progress refresh interval of `chat_turns.last_progress_at`.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// `POST /messages:stream` request.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Facts about the chat used by preflight and tool selection.
#[derive(Debug, Clone, Default)]
pub struct ChatFacts {
    pub has_ready_docs: bool,
    pub ci_file_ids: Vec<String>,
    pub vector_store_id: Option<String>,
    pub prior_context_tokens: i64,
    /// `provider_file_id -> (attachment_id, filename)` of ready attachments.
    pub file_map: HashMap<String, (Uuid, String)>,
}

/// Everything the provider task needs.
#[derive(Debug, Clone)]
pub struct TurnRuntime {
    pub ctx: SecurityContext,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub user_message_id: Uuid,
    pub user_message_created_at: super::clock::Timestamp,
    pub assistant_message_id: Uuid,
    pub chat_scope: AccessScope,
    pub child_scope: AccessScope,
    pub decision: PreflightDecision,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    pub summary_exists: bool,
    pub summary_tokens: Option<i64>,
    pub started: Instant,
}

/// Prepared (not yet committed) turn content.
#[derive(Debug, Clone)]
pub struct PreparedTurn {
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    pub summary_exists: bool,
    pub summary_tokens: Option<i64>,
}

/// `{tenant_hex}{user_hex}` (falls back to `{tenant}:{user}`).
#[must_use]
pub fn provider_user_field(tenant_id: Uuid, user_id: Uuid) -> String {
    format!("{}{}", tenant_id.as_simple(), user_id.as_simple())
}

fn turn_key(chat_id: Uuid, request_id: Uuid) -> Condition {
    Condition::all()
        .add(chat_turn::Column::ChatId.eq(chat_id))
        .add(chat_turn::Column::RequestId.eq(request_id))
}

/// Load chat facts (ready attachments, vector store, prior context tokens).
///
/// # Errors
/// DB errors.
pub async fn load_chat_facts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<ChatFacts, DomainError> {
    let ready = attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat_id))
        .filter(attachment::Column::Status.eq("ready"))
        .filter(attachment::Column::DeletedAt.is_null())
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    let mut facts = ChatFacts::default();
    for a in &ready {
        let Some(fid) = a.provider_file_id.clone() else {
            continue;
        };
        if a.attachment_kind == "document" && a.for_file_search {
            facts.has_ready_docs = true;
        }
        if a.for_code_interpreter {
            facts.ci_file_ids.push(fid.clone());
        }
        facts.file_map.insert(fid, (a.id, a.filename.clone()));
    }
    facts.vector_store_id = chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::TenantId.eq(tenant_id))
        .filter(chat_vector_store::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?
        .and_then(|r| r.vector_store_id);
    let prior = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::Role.eq("assistant"))
        .filter(message::Column::DeletedAt.is_null())
        .filter(
            Condition::any()
                .add(message::Column::InputTokens.gt(0))
                .add(message::Column::OutputTokens.gt(0)),
        )
        .order_by_desc(message::Column::CreatedAt)
        .order_by_desc(message::Column::Id)
        .limit(1)
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?;
    facts.prior_context_tokens = prior.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens));
    Ok(facts)
}

/// Recent messages (chronological) after the summary frontier, excluding the
/// current turn's request id.
///
/// # Errors
/// DB errors.
pub async fn load_recent(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    summary: Option<&thread_summary::Model>,
    exclude_request: Uuid,
    limit: u32,
) -> Result<Vec<message::Model>, DomainError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut q = message::Entity::find()
        .filter(message::Column::ChatId.eq(chat_id))
        .filter(message::Column::RequestId.is_not_null())
        .filter(message::Column::RequestId.ne(exclude_request))
        .filter(message::Column::DeletedAt.is_null())
        .filter(message::Column::IsCompressed.eq(false));
    if let Some(s) = summary {
        q = q.filter(
            Condition::any()
                .add(message::Column::CreatedAt.gt(s.summarized_up_to_created_at))
                .add(
                    Condition::all()
                        .add(message::Column::CreatedAt.eq(s.summarized_up_to_created_at))
                        .add(message::Column::Id.gt(s.summarized_up_to_message_id)),
                ),
        );
    }
    let mut rows = q
        .order_by_desc(message::Column::CreatedAt)
        .order_by_desc(message::Column::Id)
        .limit(u64::from(limit))
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    rows.reverse();
    Ok(rows)
}

impl MiniChat {
    fn quota_service(&self) -> QuotaService {
        QuotaService {
            max_output_tokens_cap: self.cfg.streaming.max_output_tokens,
            minimal_generation_floor: self.cfg.estimation_budgets.minimal_generation_floor,
            web_search_daily_quota: self.cfg.quota.web_search_daily_quota,
            code_interpreter_daily_quota: self.cfg.quota.code_interpreter_daily_quota,
            warning_threshold_pct: self.cfg.quota.warning_threshold_pct,
        }
    }

    /// Quota service view of the configuration.
    #[must_use]
    pub fn quota(&self) -> QuotaService {
        self.quota_service()
    }

    /// Resolve the chat's model in the catalog (without the enabled filter).
    ///
    /// # Errors
    /// `InvalidModel` when the model was removed from the catalog.
    pub fn check_chat_model(policy: &PolicyView, chat: &chat::Model) -> Result<(), DomainError> {
        if policy.find(&chat.model).is_none() {
            return Err(DomainError::InvalidModel(chat.model.clone()));
        }
        Ok(())
    }

    /// Image / vision / input-length guards after the cascade.
    ///
    /// # Errors
    /// `FeatureDisabled(images)`, `VisionNotSupported`, `InputTooLong`.
    pub fn post_cascade_guards(
        policy: &PolicyView,
        decision: &PreflightDecision,
        content: &str,
        images: usize,
    ) -> Result<(), DomainError> {
        let m = &decision.effective;
        if m.max_input_tokens > 0 {
            let est = estimate_text_tokens(content.len(), &m.estimation_budgets);
            if est > i64::from(m.max_input_tokens) {
                return Err(DomainError::InputTooLong { estimated: est, max: m.max_input_tokens });
            }
        }
        if images > 0 {
            if policy.snapshot.kill_switches.disable_images {
                return Err(DomainError::FeatureDisabled("images"));
            }
            if !m.supports_vision() {
                return Err(DomainError::VisionNotSupported(m.id.clone()));
            }
        }
        Ok(())
    }

    /// Context assembly + provider resolution + request building.
    ///
    /// # Errors
    /// `ContextBudgetExceeded`, provider resolution errors, DB errors.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines, reason = "turn assembly inputs")]
    pub async fn prepare_turn(
        &self,
        ctx: &SecurityContext,
        access: &ChatAccess,
        decision: &PreflightDecision,
        facts: &ChatFacts,
        request_id: Uuid,
        content: &str,
        images: &[attachment::Model],
    ) -> Result<PreparedTurn, DomainError> {
        let m = &decision.effective;
        let chat_id = access.chat.id;
        let conn = self.db.conn()?;
        let summary = thread_summary::Entity::find()
            .filter(thread_summary::Column::ChatId.eq(chat_id))
            .secure()
            .scope_with(&access.child_scope)
            .one(&conn)
            .await?;
        let recent = load_recent(
            &conn,
            &access.child_scope,
            chat_id,
            summary.as_ref(),
            request_id,
            self.cfg.context.recent_messages_limit,
        )
        .await?;
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);

        // Tools (decided from the request, attachments, kill switches and tool_support).
        let mut tools = Vec::new();
        let mut guards: Vec<&str> = Vec::new();
        let mut features: Vec<&str> = Vec::new();
        let file_search_sent = if decision.tools.file_search
            && let Some(vs) = &facts.vector_store_id
        {
            tools.push(LlmTool::FileSearch { vector_store_ids: vec![vs.clone()], max_num_results: m.max_num_results });
            guards.push(&self.cfg.context.file_search_guard);
            features.push("file_search");
            true
        } else {
            false
        };
        if decision.tools.web_search {
            tools.push(LlmTool::WebSearch { context_size: m.web_search_context_size });
            guards.push(&self.cfg.context.web_search_guard);
            features.push("web_search");
        }
        if decision.tools.code_interpreter && !facts.ci_file_ids.is_empty() {
            tools.push(LlmTool::CodeInterpreter { file_ids: facts.ci_file_ids.clone() });
            features.push("code_interpreter");
        }
        let provider = self.llm.resolve(&m.provider_id, access.chat.tenant_id)?;
        if !file_search_sent && self.knowledge_search_available(&provider, access.chat.tenant_id) {
            tools.push(LlmTool::Function {
                name: "search_knowledge".to_owned(),
                description: "Search the organization knowledge base for relevant information.".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "top_k": {"type": "integer", "description": "Number of results"}
                    },
                    "required": ["query"]
                }),
            });
            guards.push(&self.cfg.knowledge_search.guard);
        }
        let mut instructions = m.system_prompt.clone();
        for g in guards {
            if g.trim().is_empty() {
                continue;
            }
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(g);
        }
        let tool_tokens = Surcharges {
            file_search: decision.tools.file_search,
            web_search: decision.tools.web_search,
            code_interpreter: decision.tools.code_interpreter,
            images: 0,
        }
        .tool_tokens(&m.estimation_budgets);
        let history: Vec<HistoryMessage> = recent
            .iter()
            .filter(|r| r.role != "system")
            .map(|r| HistoryMessage { role: r.role.clone(), content: r.content.clone() })
            .collect();
        let images_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        let ctx_plan = plan_context(&ContextInput {
            model: m,
            max_output_tokens_applied: decision.reserve.max_output_tokens_applied,
            tool_surcharge_tokens: tool_tokens,
            instructions: &instructions,
            summary: summary.as_ref().map(|s| s.summary_text.as_str()),
            recent: &history,
            user_message: content,
            images: images_count,
        })?;

        let mut messages = Vec::new();
        if let Some(block) = &ctx_plan.summary_block {
            messages.push(LlmMessage::text(LlmRole::User, block.clone()));
        }
        for h in &ctx_plan.history {
            let role = if h.role == "assistant" { LlmRole::Assistant } else { LlmRole::User };
            messages.push(LlmMessage::text(role, h.content.clone()));
        }
        let mut parts = vec![LlmPart::Text(content.to_owned())];
        for img in images {
            if let Some(fid) = &img.provider_file_id {
                parts.push(LlmPart::Image { file_id: fid.clone(), secondary_file_id: img.secondary_file_id.clone() });
            }
        }
        messages.push(LlmMessage { role: LlmRole::User, parts });
        self.metrics.image_inputs(images.len());

        let request = LlmRequest {
            provider_model_id: m.provider_model_id.clone(),
            instructions,
            messages,
            max_output_tokens: u32::try_from(decision.reserve.max_output_tokens_applied).unwrap_or(u32::MAX),
            tools,
            max_tool_calls: m.max_tool_calls,
            api_params: m.general_config.api_params.clone(),
            user: provider_user_field(ctx.subject_tenant_id(), ctx.subject_id()),
            metadata: LlmMetadata {
                tenant_id: ctx.subject_tenant_id().to_string(),
                user_id: ctx.subject_id().to_string(),
                chat_id: chat_id.to_string(),
                request_type: "chat",
                feature: if features.is_empty() { "none".to_owned() } else { features.join("+") },
            },
            stream: true,
            extra_input: Vec::new(),
        };
        Ok(PreparedTurn {
            provider,
            request,
            file_map: if file_search_sent { facts.file_map.clone() } else { HashMap::new() },
            assembled_tokens: ctx_plan.assembled_tokens,
            effective_budget: ctx_plan.effective_budget,
            messages_truncated: ctx_plan.messages_truncated,
            summary_exists: summary.is_some(),
            summary_tokens: ctx_plan.summary_block.as_ref().map(|_| {
                summary.as_ref().map_or(ctx_plan.summary_tokens, |s| i64::from(s.token_estimate).max(0))
            }),
        })
    }

    fn knowledge_search_available(&self, provider: &ResolvedProvider, tenant_id: Uuid) -> bool {
        let ks = &self.cfg.knowledge_search;
        if !ks.enabled {
            return false;
        }
        let _ = provider;
        let Some(pid) = ks.provider_id.as_deref() else {
            return false;
        };
        let Some(entry) = self.llm.providers().get(pid) else {
            return false;
        };
        let ok = matches!(entry.kind, ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages)
            && entry.api_version.as_deref().is_some_and(|v| !v.trim().is_empty())
            && self.llm.resolve(pid, tenant_id).is_ok();
        if !ok {
            tracing::warn!("knowledge search parameters cannot be built; search_knowledge is off for this request");
        }
        ok
    }

    /// `POST /v1/chats/{id}/messages:stream`.
    ///
    /// # Errors
    /// Every pre-stream rejection of the send path (JSON errors).
    #[allow(clippy::too_many_lines, reason = "ordered setup pipeline")]
    pub async fn send_message(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid, req: SendRequest) -> Result<StreamStart, DomainError> {
        let access = self.load_chat(ctx, actions::SEND_MESSAGE, chat_id).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        Self::check_chat_model(&policy, &access.chat)?;
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let conn = self.db.conn()?;

        // 1. Idempotency (before the parallel-turn guard).
        if let Some(t) = chat_turn::Entity::find()
            .filter(turn_key(chat_id, request_id))
            .secure()
            .scope_with(&access.child_scope)
            .one(&conn)
            .await?
        {
            if t.deleted_at.is_some() {
                return Err(DomainError::RequestIdConflict(format!("turn {} for request {request_id} is deleted", t.id)));
            }
            if t.state == TurnState::Completed.as_str() {
                return self.replay(&conn, &access, &t).await.map(StreamStart::Replay);
            }
            return Err(DomainError::RequestIdConflict(format!("turn {} for request {request_id} is {}", t.id, t.state)));
        }
        // 2. Parallel turn guard.
        if chat_turn::Entity::find()
            .filter(chat_turn::Column::ChatId.eq(chat_id))
            .filter(chat_turn::Column::State.eq("running"))
            .filter(chat_turn::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&access.child_scope)
            .one(&conn)
            .await?
            .is_some()
        {
            return Err(DomainError::TurnAlreadyRunning);
        }
        // 3. Attachment ids: count / duplicates / image count.
        let max_ids = (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if req.attachment_ids.len() > max_ids {
            return Err(DomainError::InvalidAttachment(format!("at most {max_ids} attachments per message")));
        }
        let mut seen = HashSet::new();
        for id in &req.attachment_ids {
            if !seen.insert(*id) {
                return Err(DomainError::InvalidAttachment(format!("duplicate attachment id {id}")));
            }
        }
        let referenced = if req.attachment_ids.is_empty() {
            Vec::new()
        } else {
            attachment::Entity::find()
                .filter(attachment::Column::ChatId.eq(chat_id))
                .filter(attachment::Column::Id.is_in(req.attachment_ids.clone()))
                .filter(attachment::Column::DeletedAt.is_null())
                .filter(attachment::Column::UploadedByUserId.eq(ctx.subject_id()))
                .secure()
                .scope_with(&access.child_scope)
                .all(&conn)
                .await?
        };
        let images: Vec<attachment::Model> =
            referenced.iter().filter(|a| a.attachment_kind == "image").cloned().collect();
        if images.len() > self.cfg.rag.max_images_per_message as usize {
            return Err(DomainError::TooManyImages { count: images.len(), max: self.cfg.rag.max_images_per_message });
        }
        // 4. Kill switch for web search (before the cascade).
        if req.web_search && policy.snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        // 5. Preflight.
        let facts = load_chat_facts(&conn, &access.child_scope, access.chat.tenant_id, chat_id).await?;
        let limits = self.policy.user_limits(ctx.subject_id(), policy.version).await?;
        let inputs = QuotaInputs {
            message_bytes: req.content.len(),
            prior_context_tokens: facts.prior_context_tokens,
            images: u32::try_from(images.len()).unwrap_or(u32::MAX),
            has_ready_docs: facts.has_ready_docs,
            has_ready_code_files: !facts.ci_file_ids.is_empty(),
            web_search_requested: req.web_search,
        };
        let decision = match self
            .quota_service()
            .preflight(&conn, &access.scope, access.chat.tenant_id, ctx.subject_id(), &access.chat.model, &policy, limits, &inputs)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                self.metrics.quota_preflight("reject", &access.chat.model, "none");
                return Err(e);
            }
        };
        self.metrics.quota_preflight(decision.quota_decision(), &decision.effective.id, decision.effective.tier.as_str());
        self.metrics.quota_estimated_tokens(decision.reserve.reserve_tokens);
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        // 6. Post-cascade guards.
        Self::post_cascade_guards(&policy, &decision, &req.content, images.len())?;
        // 7. Context assembly + provider resolution.
        let prepared = self.prepare_turn(ctx, &access, &decision, &facts, request_id, &req.content, &images).await?;
        // 8. Reserve transaction.
        let turn = self
            .commit_send(ctx, &access, &decision, request_id, &req, &prepared)
            .await?;
        self.metrics.quota_reserve();
        Ok(StreamStart::Live(self.start_live(ctx, &access, decision, prepared, turn)))
    }

    #[allow(clippy::too_many_lines, reason = "single reserve transaction")]
    async fn commit_send(
        &self,
        ctx: &SecurityContext,
        access: &ChatAccess,
        decision: &PreflightDecision,
        request_id: Uuid,
        req: &SendRequest,
        prepared: &PreparedTurn,
    ) -> Result<CommittedTurn, DomainError> {
        let quota = self.quota_service();
        let scope = access.scope.clone();
        let child = access.child_scope.clone();
        let tenant_id = access.chat.tenant_id;
        let user_id = ctx.subject_id();
        let chat_id = access.chat.id;
        let decision = decision.clone();
        let content = req.content.clone();
        let attachment_ids = req.attachment_ids.clone();
        let web_search = req.web_search;
        let _ = prepared;
        let turn_id = Uuid::now_v7();
        let user_message_id = Uuid::now_v7();
        let now = clock::now();
        let result = self
            .tx(move |tx| {
                Box::pin(async move {
                    quota.reserve(tx, &scope, tenant_id, user_id, &decision).await?;
                    let msg = message::ActiveModel {
                        id: Set(user_message_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(Some(request_id)),
                        role: Set("user".to_owned()),
                        content: Set(content),
                        content_type: Set("text".to_owned()),
                        token_estimate: Set(0),
                        provider_response_id: Set(None),
                        request_kind: Set("chat".to_owned()),
                        features_used: Set(json!([])),
                        input_tokens: Set(0),
                        output_tokens: Set(0),
                        cache_read_input_tokens: Set(0),
                        cache_write_input_tokens: Set(0),
                        reasoning_tokens: Set(0),
                        model: Set(None),
                        is_compressed: Set(false),
                        created_at: Set(now),
                        deleted_at: Set(None),
                    };
                    secure_insert::<message::Entity>(msg, &child, tx).await?;
                    chat::Entity::update_many()
                        .secure()
                        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
                        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if !attachment_ids.is_empty() {
                        let rows = attachment::Entity::find()
                            .filter(attachment::Column::ChatId.eq(chat_id))
                            .filter(attachment::Column::Id.is_in(attachment_ids.clone()))
                            .filter(attachment::Column::DeletedAt.is_null())
                            .filter(attachment::Column::TenantId.eq(tenant_id))
                            .filter(attachment::Column::UploadedByUserId.eq(user_id))
                            .filter(attachment::Column::Status.eq("ready"))
                            .secure()
                            .scope_with(&child)
                            .all(tx)
                            .await?;
                        if rows.len() != attachment_ids.len() {
                            return Err(DomainError::InvalidAttachment(
                                "attachment not found, not ready, or not owned by the caller".to_owned(),
                            ));
                        }
                        for aid in &attachment_ids {
                            let link = message_attachment::ActiveModel {
                                tenant_id: Set(tenant_id),
                                chat_id: Set(chat_id),
                                message_id: Set(user_message_id),
                                attachment_id: Set(*aid),
                                created_at: Set(now),
                            };
                            secure_insert::<message_attachment::Entity>(link, &child, tx).await?;
                        }
                    }
                    let turn = chat_turn::ActiveModel {
                        id: Set(turn_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(request_id),
                        requester_type: Set("user".to_owned()),
                        requester_user_id: Set(Some(user_id)),
                        state: Set("running".to_owned()),
                        provider_name: Set(None),
                        provider_response_id: Set(None),
                        assistant_message_id: Set(None),
                        error_code: Set(None),
                        reserve_tokens: Set(Some(decision.reserve.reserve_tokens)),
                        max_output_tokens_applied: Set(Some(i32::try_from(decision.reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        reserved_credits_micro: Set(Some(decision.reserved_credits_micro)),
                        policy_version_applied: Set(Some(i64::try_from(decision.policy_version).unwrap_or(i64::MAX))),
                        effective_model: Set(Some(decision.effective.id.clone())),
                        minimal_generation_floor_applied: Set(Some(i32::try_from(decision.floor_applied).unwrap_or(i32::MAX))),
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
                    secure_insert::<chat_turn::Entity>(turn, &child, tx).await?;
                    Ok(CommittedTurn { turn_id, user_message_id, user_message_created_at: now, request_id })
                })
            })
            .await;
        match result {
            Ok(t) => Ok(t),
            Err(DomainError::UniqueViolation(detail)) => {
                // Lost an insert race: report request_id_conflict when a turn
                // with this request id exists, otherwise turn_already_running.
                let conn = self.db.conn()?;
                let exists = chat_turn::Entity::find()
                    .filter(turn_key(access.chat.id, request_id))
                    .secure()
                    .scope_with(&access.child_scope)
                    .one(&conn)
                    .await?
                    .is_some();
                if exists {
                    Err(DomainError::RequestIdConflict(detail))
                } else {
                    Err(DomainError::TurnAlreadyRunning)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Build the replay events of a completed turn (no side effects).
    async fn replay(&self, runner: &impl DBRunner, access: &ChatAccess, t: &chat_turn::Model) -> Result<Vec<StreamEvent>, DomainError> {
        let mut q = message::Entity::find()
            .filter(message::Column::ChatId.eq(access.chat.id))
            .filter(message::Column::DeletedAt.is_null());
        q = if let Some(mid) = t.assistant_message_id {
            q.filter(message::Column::Id.eq(mid))
        } else {
            q.filter(message::Column::RequestId.eq(t.request_id)).filter(message::Column::Role.eq("assistant"))
        };
        let msg = q
            .secure()
            .scope_with(&access.child_scope)
            .one(runner)
            .await?
            .ok_or_else(|| DomainError::Internal(format!("completed turn {} has no assistant message", t.id)))?;
        let selected = access.chat.model.clone();
        let effective = t.effective_model.clone().or_else(|| msg.model.clone()).unwrap_or_else(|| selected.clone());
        let downgraded = effective != selected;
        Ok(vec![
            StreamEvent::Started(StreamStartedData {
                request_id: t.request_id,
                message_id: msg.id,
                is_new_turn: false,
                thread_summary_applied: None,
            }),
            StreamEvent::Delta(DeltaData { kind: "text", content: msg.content.clone() }),
            StreamEvent::Done(DoneData {
                usage: UsageData { input_tokens: msg.input_tokens, output_tokens: msg.output_tokens },
                effective_model: effective,
                selected_model: selected.clone(),
                quota_decision: if downgraded { "downgrade" } else { "allow" },
                downgrade_from: downgraded.then_some(selected),
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ])
    }

    /// Spawn the provider task and return the live stream handle.
    #[must_use]
    pub fn start_live(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        access: &ChatAccess,
        decision: PreflightDecision,
        prepared: PreparedTurn,
        turn: CommittedTurn,
    ) -> LiveStream {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let assistant_message_id = Uuid::now_v7();
        let rt = TurnRuntime {
            ctx: ctx.clone(),
            tenant_id: access.chat.tenant_id,
            user_id: ctx.subject_id(),
            chat_id: access.chat.id,
            turn_id: turn.turn_id,
            request_id: turn.request_id,
            user_message_id: turn.user_message_id,
            user_message_created_at: turn.user_message_created_at,
            assistant_message_id,
            chat_scope: access.scope.clone(),
            child_scope: access.child_scope.clone(),
            decision,
            provider: prepared.provider,
            request: prepared.request,
            file_map: prepared.file_map,
            assembled_tokens: prepared.assembled_tokens,
            effective_budget: prepared.effective_budget,
            messages_truncated: prepared.messages_truncated,
            summary_exists: prepared.summary_exists,
            summary_tokens: prepared.summary_tokens,
            started: Instant::now(),
        };
        let started = StreamEvent::Started(StreamStartedData {
            request_id: rt.request_id,
            message_id: assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: rt.summary_tokens.map(|t| ThreadSummaryInfo { token_estimate: t }),
        });
        // The channel has capacity >= 16, so the first event never blocks.
        if let Err(e) = tx.try_send(started) {
            tracing::debug!(error = %e, "failed to enqueue the started event");
        }
        let svc = Arc::clone(self);
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            svc.metrics.stream_started(&rt.provider.provider_id, &rt.decision.effective.id);
            svc.run_turn(rt, tx, task_cancel).await;
            svc.metrics.stream_ended();
        });
        LiveStream {
            rx,
            guard: CancelOnDrop(cancel),
            ping_interval_secs: u64::from(self.cfg.streaming.sse_ping_interval_seconds),
        }
    }
}

/// The committed turn identity.
#[derive(Debug, Clone, Copy)]
pub struct CommittedTurn {
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    pub user_message_created_at: super::clock::Timestamp,
    pub request_id: Uuid,
}

/// Mutable state of the provider task.
#[derive(Debug, Default)]
struct RunState {
    text: String,
    web_search_started: u32,
    code_interpreter_started: u32,
    web_search_completed: u32,
    code_interpreter_completed: u32,
    file_search_completed: u32,
    knowledge_calls: u32,
    iterations: u32,
    first_token_at: Option<Instant>,
    content_sent: bool,
}

enum Step {
    Continue,
    Finish(TerminalOutcome),
    Disconnected,
}

impl MiniChat {
    async fn emit(tx: &mpsc::Sender<StreamEvent>, cancel: &CancellationToken, ev: StreamEvent) -> bool {
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            r = tx.send(ev) => r.is_ok(),
        }
    }

    async fn touch_progress(&self, rt: &TurnRuntime, st: &RunState) {
        let Ok(conn) = self.db.conn() else {
            return;
        };
        let now = clock::now();
        let res = chat_turn::Entity::update_many()
            .secure()
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::WebSearchCompletedCount, Expr::value(i32::try_from(st.web_search_completed).unwrap_or(i32::MAX)))
            .col_expr(
                chat_turn::Column::CodeInterpreterCompletedCount,
                Expr::value(i32::try_from(st.code_interpreter_completed).unwrap_or(i32::MAX)),
            )
            .col_expr(chat_turn::Column::FileSearchCompletedCount, Expr::value(i32::try_from(st.file_search_completed).unwrap_or(i32::MAX)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .filter(Condition::all().add(chat_turn::Column::Id.eq(rt.turn_id)).add(chat_turn::Column::State.eq("running")))
            .scope_with(&rt.child_scope)
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, turn_id = %rt.turn_id, "progress update failed");
        }
    }

    /// The provider task: relay provider events, enforce per-turn tool
    /// limits, then finalize exactly once.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, reason = "event loop")]
    async fn run_turn(self: &Arc<Self>, mut rt: TurnRuntime, tx: mpsc::Sender<StreamEvent>, cancel: CancellationToken) {
        let mut st = RunState::default();
        let mut last_progress = Instant::now();
        let provider_id = rt.provider.provider_id.clone();
        let model_id = rt.decision.effective.id.clone();
        let outcome: Option<TerminalOutcome> = 'outer: loop {
            st.iterations += 1;
            let request_started = Instant::now();
            let mut stream = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.metrics.cancel_requested();
                    self.metrics.stream_disconnected("before_first_token");
                    self.metrics.cancel_effective(0.0);
                    break 'outer Some(TerminalOutcome::Cancelled { partial_text: st.text.clone() });
                }
                s = self.llm.stream_chat(&rt.ctx, &rt.provider, &rt.request) => s,
            };
            loop {
                let ev = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        let t0 = Instant::now();
                        drop(stream);
                        self.metrics.cancel_requested();
                        self.metrics.stream_disconnected(if st.content_sent { "mid_stream" } else { "before_first_token" });
                        self.metrics.cancel_effective(t0.elapsed().as_secs_f64() * 1000.0);
                        break 'outer Some(TerminalOutcome::Cancelled { partial_text: st.text.clone() });
                    }
                    ev = stream.next() => ev,
                };
                let step = match ev {
                    None => Step::Finish(TerminalOutcome::Failed {
                        code: codes::PROVIDER_ERROR.to_owned(),
                        message: "Provider stream ended unexpectedly".to_owned(),
                        usage: None,
                        response_id: None,
                    }),
                    Some(ProviderEvent::TextDelta(t)) => {
                        if st.first_token_at.is_none() {
                            let now = Instant::now();
                            st.first_token_at = Some(now);
                            let provider_ms = now.duration_since(request_started).as_secs_f64() * 1000.0;
                            self.metrics.ttft(&provider_id, &model_id, provider_ms, 0.0);
                        }
                        st.text.push_str(&t);
                        st.content_sent = true;
                        if Self::emit(&tx, &cancel, StreamEvent::Delta(DeltaData { kind: "text", content: t })).await {
                            Step::Continue
                        } else {
                            Step::Disconnected
                        }
                    }
                    Some(ProviderEvent::ReasoningDelta(t)) => {
                        st.content_sent = true;
                        if Self::emit(&tx, &cancel, StreamEvent::Delta(DeltaData { kind: "reasoning", content: t })).await {
                            Step::Continue
                        } else {
                            Step::Disconnected
                        }
                    }
                    Some(ProviderEvent::ToolStart { name, details }) => {
                        let mut limit_hit = None;
                        if name == "web_search" {
                            st.web_search_started += 1;
                            if st.web_search_started > self.cfg.quota.web_search_max_calls_per_message {
                                limit_hit = Some(codes::WEB_SEARCH_CALLS_EXCEEDED);
                            }
                        } else if name == "code_interpreter" {
                            st.code_interpreter_started += 1;
                            if st.code_interpreter_started > self.cfg.quota.code_interpreter_max_calls_per_message {
                                limit_hit = Some(codes::CODE_INTERPRETER_CALLS_EXCEEDED);
                            }
                        }
                        if let Some(code) = limit_hit {
                            drop(stream);
                            let message = if code == codes::WEB_SEARCH_CALLS_EXCEEDED {
                                "Web search call limit per message exceeded"
                            } else {
                                "Code interpreter call limit per message exceeded"
                            };
                            break 'outer Some(TerminalOutcome::Failed {
                                code: code.to_owned(),
                                message: message.to_owned(),
                                usage: None,
                                response_id: None,
                            });
                        }
                        st.content_sent = true;
                        if last_progress.elapsed() >= PROGRESS_EVERY {
                            last_progress = Instant::now();
                            self.touch_progress(&rt, &st).await;
                        }
                        if Self::emit(&tx, &cancel, StreamEvent::Tool(ToolData { phase: "start", name, details })).await {
                            Step::Continue
                        } else {
                            Step::Disconnected
                        }
                    }
                    Some(ProviderEvent::ToolDone { name, details }) => {
                        match name.as_str() {
                            "web_search" => st.web_search_completed += 1,
                            "code_interpreter" => st.code_interpreter_completed += 1,
                            "file_search" => st.file_search_completed += 1,
                            _ => {}
                        }
                        st.content_sent = true;
                        last_progress = Instant::now();
                        self.touch_progress(&rt, &st).await;
                        if Self::emit(&tx, &cancel, StreamEvent::Tool(ToolData { phase: "done", name, details })).await {
                            Step::Continue
                        } else {
                            Step::Disconnected
                        }
                    }
                    Some(ProviderEvent::FunctionCall { call_id, name, arguments, raw_item }) => {
                        drop(stream);
                        match self.handle_function_call(&mut rt, &mut st, &call_id, &name, &arguments, raw_item).await {
                            Ok(()) => continue 'outer,
                            Err(outcome) => break 'outer Some(outcome),
                        }
                    }
                    Some(ProviderEvent::Completed { usage, response_id, incomplete_reason, citations, output_text }) => {
                        let text = if st.text.is_empty() { output_text.unwrap_or_default() } else { st.text.clone() };
                        if let Some(r) = &incomplete_reason {
                            tracing::warn!(reason = %r, turn_id = %rt.turn_id, "stream incomplete");
                            self.metrics.stream_incomplete(&provider_id, &model_id, r);
                        }
                        let mapped = map_citations(&citations, &rt.file_map);
                        Step::Finish(TerminalOutcome::Completed { usage, response_id, incomplete_reason, text, citations: mapped })
                    }
                    Some(ProviderEvent::Failed { kind, message, usage, response_id, .. }) => {
                        Step::Finish(TerminalOutcome::Failed { code: kind.code().to_owned(), message, usage, response_id })
                    }
                };
                match step {
                    Step::Continue => {
                        if last_progress.elapsed() >= PROGRESS_EVERY {
                            last_progress = Instant::now();
                            self.touch_progress(&rt, &st).await;
                        }
                    }
                    Step::Finish(o) => break 'outer Some(o),
                    Step::Disconnected => {
                        drop(stream);
                        self.metrics.stream_disconnected("mid_stream");
                        break 'outer Some(TerminalOutcome::Cancelled { partial_text: st.text.clone() });
                    }
                }
            }
        };
        let Some(outcome) = outcome else {
            return;
        };
        let counts = super::finalize::RunStats {
            web_search: st.web_search_completed,
            code_interpreter: st.code_interpreter_completed,
            file_search: st.file_search_completed,
            reported_file_search: st.knowledge_calls,
            ttft_ms: st
                .first_token_at
                .map(|t| u64::try_from(t.duration_since(rt.started).as_millis()).unwrap_or(u64::MAX)),
        };
        let is_cancel = matches!(outcome, TerminalOutcome::Cancelled { .. });
        let done_template = DoneData {
            usage: UsageData::default(),
            effective_model: rt.decision.effective.id.clone(),
            selected_model: rt.decision.selected_model.clone(),
            quota_decision: rt.decision.quota_decision(),
            downgrade_from: (rt.decision.quota_decision() == "downgrade").then(|| rt.decision.selected_model.clone()),
            downgrade_reason: rt.decision.downgrade_reason.map(str::to_owned),
            quota_warnings: None,
        };
        let citations = match &outcome {
            TerminalOutcome::Completed { citations, .. } => citations.clone(),
            _ => Vec::new(),
        };
        let failure = match &outcome {
            TerminalOutcome::Failed { code, message, .. } => Some((code.clone(), message.clone())),
            _ => None,
        };
        let usage = match &outcome {
            TerminalOutcome::Completed { usage, .. } => usage.unwrap_or_default(),
            _ => ProviderUsage::default(),
        };
        let total_ms = rt.started.elapsed().as_secs_f64() * 1000.0;
        let result = self.finalize(&rt, outcome, counts).await;
        self.metrics.total_latency(&provider_id, &model_id, total_ms);
        if is_cancel {
            return;
        }
        match result {
            FinalizeResult::Won { quota_warnings } => {
                if let Some((code, message)) = failure {
                    self.metrics.stream_failed(&provider_id, &model_id, &code);
                    let _ = Self::emit(&tx, &cancel, StreamEvent::error(&code, message)).await;
                    return;
                }
                self.metrics.stream_completed(&provider_id, &model_id);
                if !citations.is_empty()
                    && !Self::emit(&tx, &cancel, StreamEvent::Citations(CitationsData { items: citations })).await
                {
                    return;
                }
                let done = DoneData {
                    usage: UsageData { input_tokens: usage.input_tokens, output_tokens: usage.output_tokens },
                    quota_warnings: Some(quota_warnings),
                    ..done_template
                };
                let _ = Self::emit(&tx, &cancel, StreamEvent::Done(done)).await;
            }
            FinalizeResult::PersistenceFailed => {
                self.metrics.stream_failed(&provider_id, &model_id, codes::MESSAGE_PERSISTENCE_FAILED);
                let _ = Self::emit(
                    &tx,
                    &cancel,
                    StreamEvent::error(codes::MESSAGE_PERSISTENCE_FAILED, "The answer could not be saved"),
                )
                .await;
            }
            FinalizeResult::TxFailed => {
                let (code, message) = failure.unwrap_or_else(|| {
                    (codes::FINALIZATION_FAILED.to_owned(), "The turn could not be finalized".to_owned())
                });
                self.metrics.stream_failed(&provider_id, &model_id, &code);
                let _ = Self::emit(&tx, &cancel, StreamEvent::error(&code, message)).await;
            }
            FinalizeResult::Lost => {
                // No terminal event: the relay synthesizes `stream_interrupted`.
            }
        }
    }

    /// Knowledge-search agentic loop step (function tool call).
    #[allow(clippy::result_large_err, reason = "the error is the terminal outcome of the turn")]
    async fn handle_function_call(
        &self,
        rt: &mut TurnRuntime,
        st: &mut RunState,
        call_id: &str,
        name: &str,
        arguments: &str,
        raw_item: Value,
    ) -> Result<(), TerminalOutcome> {
        let ks = &self.cfg.knowledge_search;
        let ks_offered = rt.request.tools.iter().any(|t| matches!(t, LlmTool::Function { name, .. } if name == "search_knowledge"));
        if !ks_offered || name != "search_knowledge" {
            return Err(TerminalOutcome::Failed {
                code: codes::UNEXPECTED_TOOL_USE.to_owned(),
                message: "The model requested a tool that is not available".to_owned(),
                usage: None,
                response_id: None,
            });
        }
        if st.iterations > ks.max_calls_per_message + 2 {
            return Err(TerminalOutcome::Failed {
                code: codes::AGENTIC_ITERATIONS_EXCEEDED.to_owned(),
                message: "Tool-use iteration limit exceeded".to_owned(),
                usage: None,
                response_id: None,
            });
        }
        let output = if st.knowledge_calls >= ks.max_calls_per_message {
            "search limit reached; answer from the information you already have".to_owned()
        } else {
            st.knowledge_calls += 1;
            let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
            let query = args.get("query").and_then(Value::as_str).unwrap_or_default().to_owned();
            let top_k = args
                .get("top_k")
                .and_then(Value::as_u64)
                .and_then(|k| usize::try_from(k).ok())
                .unwrap_or(ks.top_k)
                .clamp(1, ks.top_k);
            match self.knowledge_retrieve(rt, &query, top_k).await {
                Ok(chunks) => {
                    st.file_search_completed += 1;
                    chunks
                }
                Err(e) => format!("search failed: {e}"),
            }
        };
        match rt.provider.kind {
            ProviderKind::OpenaiChatCompletions => {
                rt.request.extra_input.push(raw_item);
                rt.request.extra_input.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            ProviderKind::AnthropicMessages => {
                rt.request.extra_input.push(raw_item);
                rt.request.extra_input.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": call_id, "content": output}]}));
            }
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
                rt.request.extra_input.push(raw_item);
                rt.request.extra_input.push(json!({"type": "function_call_output", "call_id": call_id, "output": output}));
            }
        }
        Ok(())
    }

    async fn knowledge_retrieve(&self, rt: &TurnRuntime, query: &str, top_k: usize) -> Result<String, String> {
        let ks = &self.cfg.knowledge_search;
        let pid = ks.provider_id.clone().unwrap_or_default();
        let vs = ks.vector_store_id.clone().unwrap_or_default();
        let entry = self.llm.providers().get(&pid).cloned().ok_or("knowledge provider missing")?;
        let provider = self.llm.resolve(&pid, rt.tenant_id).map_err(|e| e.to_string())?;
        let version = entry.api_version.unwrap_or_default();
        let uri = format!("/{}/openai/vector_stores/{vs}/search?api-version={version}", provider.alias);
        let started = Instant::now();
        let v = self
            .llm
            .post_raw(&rt.ctx, uri, json!({"query": query, "max_num_results": top_k}))
            .await
            .map_err(|e| e.to_string())?;
        let _ = started;
        let mut out = String::new();
        if let Some(data) = v.get("data").and_then(Value::as_array) {
            for (i, item) in data.iter().take(top_k).enumerate() {
                let mut text = String::new();
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for c in content {
                        text.push_str(c.get("text").and_then(Value::as_str).unwrap_or_default());
                    }
                }
                let text: String = text.chars().take(ks.max_chunk_chars).collect();
                let fname = item.get("filename").and_then(Value::as_str).unwrap_or_default();
                let entry = format!("[{}] {fname}\n{text}\n\n", i + 1);
                out.push_str(&entry);
            }
        }
        if out.is_empty() {
            out.push_str("no results");
        }
        Ok(out)
    }
}

/// Map provider annotations to client citations (file ids resolved to
/// attachments of the chat; unknown or deleted files omitted).
#[must_use]
pub fn map_citations<S: std::hash::BuildHasher>(
    raw: &[RawCitation],
    file_map: &HashMap<String, (Uuid, String), S>,
) -> Vec<Citation> {
    raw.iter()
        .filter_map(|c| match c {
            RawCitation::Web { url, title, snippet, span } => Some(Citation {
                source: "web",
                title: title.clone(),
                url: Some(url.clone()),
                attachment_id: None,
                snippet: snippet.clone(),
                span: span.map(|(start, end)| TextSpan { start, end }),
            }),
            RawCitation::File { file_id, .. } => file_map.get(file_id).map(|(aid, fname)| Citation {
                source: "file",
                title: fname.clone(),
                url: None,
                attachment_id: Some(*aid),
                snippet: String::new(),
                span: None,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_field_is_64_hex_chars() {
        let f = provider_user_field(Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(f.len(), 64);
        assert!(f.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn citation_mapping_hides_provider_ids() {
        let aid = Uuid::new_v4();
        let mut map = HashMap::new();
        map.insert("file-known".to_owned(), (aid, "Q3.pdf".to_owned()));
        let raw = vec![
            RawCitation::File { file_id: "file-known".into(), filename: Some("x".into()) },
            RawCitation::File { file_id: "file-unknown".into(), filename: None },
            RawCitation::Web { url: "https://e.com".into(), title: "E".into(), snippet: "s".into(), span: Some((1, 2)) },
        ];
        let out = map_citations(&raw, &map);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].attachment_id, Some(aid));
        assert_eq!(out[0].title, "Q3.pdf");
        assert_eq!(out[0].snippet, "");
        assert!(out[0].span.is_none());
        assert_eq!(out[1].source, "web");
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("file-known"));
        assert!(!json.contains("score"));
    }
}
