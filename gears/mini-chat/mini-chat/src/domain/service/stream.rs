//! Send message, idempotent replay and the provider relay task
//! (DESIGN §3.3 Streaming Contract, §3.6 Send Message, §4 Turn Lifecycle).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{ModelTier, PolicySnapshot, UserLimits};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, SecureInsertExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::finalize::{Outcome, ToolCounters};
use super::{MiniChatService, now};
use crate::domain::authz::{ChatScopes, actions};
use crate::domain::billing::ReserveFields;
use crate::domain::context::{ContextInput, ContextPlan, HistoryMessage, HistoryRole, assemble, input_too_long};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::domain::quota::{
    CascadeOutcome, PreflightRequest, QuotaDecision, load_usage, reserve_and_recheck, run_cascade,
};
use crate::infra::db::entities::{attachments, chat_turns, chat_vector_stores, chats, message_attachments, messages, thread_summaries};
use crate::infra::llm::types::{ChatRequest, ContentPart, InputMessage, ProviderEvent, Role, StreamErrorCode, ToolSpec};
use crate::infra::llm::{ChatTarget, ProviderStream};

/// Client-facing SSE events (domain representation).
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    StreamStarted { request_id: Uuid, message_id: Uuid, is_new_turn: bool, thread_summary_applied: Option<i64> },
    Ping,
    Delta { kind: &'static str, content: String },
    Tool { phase: &'static str, name: String, details: Value },
    Citations(Vec<Citation>),
    Done(DonePayload),
    Error { code: String, message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWarningView {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DonePayload {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: &'static str,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<QuotaWarningView>>,
}

/// Body of a send request.
#[derive(Debug, Clone)]
pub struct SendInput {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Event channel handed to the SSE writer; dropping `guard` cancels the turn.
pub struct TurnStream {
    pub rx: mpsc::Receiver<SseEvent>,
    pub guard: CancelOnDrop,
}

/// Cancels the token when dropped (client disconnect).
pub struct CancelOnDrop(pub CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Everything decided before the reserve transaction.
pub(crate) struct TurnPlan {
    pub snapshot: Arc<PolicySnapshot>,
    pub limits: UserLimits,
    pub cascade: CascadeOutcome,
    pub context: ContextPlan,
    pub target: ChatTarget,
    pub request: ChatRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub floor_applied: u32,
    pub summary_exists: bool,
    pub summary_tokens: Option<i64>,
    pub started_at: OffsetDateTime,
}

/// State of a running turn passed to the relay task.
pub(crate) struct LiveTurn {
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub chat_id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub assistant_message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub tier: ModelTier,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<String>,
    pub reserve: ReserveFields,
    pub policy_version: u64,
    pub started_at: OffsetDateTime,
    pub target: ChatTarget,
    pub request: ChatRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub thread_summary_applied: Option<i64>,
    pub context_truncated: bool,
    pub assembled_tokens: i64,
    pub effective_budget: Option<i64>,
    pub summary_exists: bool,
    pub limits: UserLimits,
    pub in_mult: i64,
    pub out_mult: i64,
}

pub(crate) fn empty_content() -> DomainError {
    DomainError::invalid(Res::Chat, "content", "EMPTY_CONTENT", "content must not be empty")
}

pub(crate) fn invalid_attachment(desc: &str) -> DomainError {
    DomainError::invalid(Res::Chat, "attachment", "invalid_attachment", desc)
}

fn simple(u: Uuid) -> String {
    u.as_simple().to_string()
}

impl MiniChatService {
    /// Preliminary checks of `attachment_ids` that need no query.
    pub(crate) fn check_attachment_list(&self, ids: &[Uuid]) -> DomainResult<()> {
        let max = self.cfg.rag.max_documents_per_chat as usize + self.cfg.rag.max_images_per_message as usize;
        if ids.len() > max {
            return Err(invalid_attachment("too many attachment_ids"));
        }
        let mut seen = HashSet::new();
        if !ids.iter().all(|id| seen.insert(*id)) {
            return Err(invalid_attachment("duplicate attachment_ids"));
        }
        Ok(())
    }

    /// Load and validate the referenced attachments (owner, chat, ready).
    pub(crate) async fn load_message_attachments(
        &self,
        runner: &impl DBRunner,
        scopes: &ChatScopes,
        chat_id: Uuid,
        user_id: Uuid,
        ids: &[Uuid],
        strict: bool,
    ) -> DomainResult<Vec<attachments::Model>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat_id))
                    .add(attachments::Column::Id.is_in(ids.to_vec()))
                    .add(attachments::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scopes.tenant)
            .all(runner)
            .await?;
        let by_id: HashMap<Uuid, attachments::Model> = rows.into_iter().map(|a| (a.id, a)).collect();
        let mut out = Vec::new();
        for id in ids {
            match by_id.get(id) {
                Some(a) if a.uploaded_by_user_id == user_id && a.status == "ready" => out.push(a.clone()),
                _ if strict => return Err(invalid_attachment("attachment is not available")),
                _ => {}
            }
        }
        Ok(out)
    }

    /// Prior context proxy: tokens of the latest non-deleted assistant message with usage.
    async fn prior_context_tokens(&self, runner: &impl DBRunner, scopes: &ChatScopes, chat_id: Uuid) -> DomainResult<i64> {
        let row = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::Role.eq("assistant"))
                    .add(messages::Column::DeletedAt.is_null())
                    .add(Condition::any().add(messages::Column::InputTokens.gt(0)).add(messages::Column::OutputTokens.gt(0))),
            )
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .secure()
            .scope_with(&scopes.tenant)
            .one(runner)
            .await?;
        Ok(row.map_or(0, |m| m.input_tokens + m.output_tokens))
    }

    /// Plan a turn: preflight cascade, guards, context and provider request.
    #[allow(clippy::too_many_arguments, clippy::cognitive_complexity)] // single preflight pipeline over request inputs
    pub(crate) async fn plan_turn(
        &self,
        ctx: &SecurityContext,
        scopes: &ChatScopes,
        chat: &chats::Model,
        content: &str,
        attached: &[attachments::Model],
        web_search: bool,
        exclude_request_id: Option<Uuid>,
        request_id: Uuid,
    ) -> DomainResult<TurnPlan> {
        let conn = self.db.conn()?;
        let snapshot = self.snapshot(ctx).await?;
        let selected = Self::chat_model(&snapshot, chat)?;
        let ks = snapshot.kill_switches.clone();
        if web_search && ks.disable_web_search {
            return Err(DomainError::feature_disabled("web_search"));
        }
        let images: Vec<&attachments::Model> = attached.iter().filter(|a| a.attachment_kind == "image").collect();
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        if image_count > self.cfg.rag.max_images_per_message {
            return Err(DomainError::out_of_range(Res::Chat, "image_count", "TOO_MANY_IMAGES", "too many images in one message"));
        }

        // Chat facts.
        let chat_atts = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat.id))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::Status.eq("ready")),
            )
            .secure()
            .scope_with(&scopes.tenant)
            .all(&conn)
            .await?;
        let has_docs = chat_atts.iter().any(|a| a.attachment_kind == "document" && a.for_file_search);
        let ci_files: Vec<String> = chat_atts
            .iter()
            .filter(|a| a.for_code_interpreter)
            .filter_map(|a| a.provider_file_id.clone())
            .collect();
        let prior = self.prior_context_tokens(&conn, scopes, chat.id).await?;

        let started_at = now();
        let limits = self.policy.user_limits(ctx.subject_id(), snapshot.policy_version).await?;
        let usage = load_usage(&conn, ctx.subject_tenant_id(), ctx.subject_id(), started_at).await?;
        let pre = PreflightRequest {
            message_bytes: content.len(),
            prior_context_tokens: prior,
            image_count,
            has_ready_documents: has_docs,
            has_ready_code_interpreter_files: !ci_files.is_empty(),
            web_search_requested: web_search,
            max_output_cap: self.cfg.streaming.max_output_tokens,
            floor: self.cfg.estimation_budgets.minimal_generation_floor,
        };
        let cascade = run_cascade(&snapshot, &selected, &usage, &limits, &pre)?;
        let gates = cascade.reserve.gates;
        let daily = usage.get(&("daily", crate::domain::quota::BUCKET_TOTAL)).copied().unwrap_or_default();
        if gates.web_search && daily.web_search_calls >= i64::from(self.cfg.quota.web_search_daily_quota) {
            return Err(DomainError::QuotaExceeded { scope: "web_search".to_owned() });
        }
        if gates.code_interpreter && daily.code_interpreter_calls >= i64::from(self.cfg.quota.code_interpreter_daily_quota) {
            return Err(DomainError::QuotaExceeded { scope: "code_interpreter".to_owned() });
        }
        let eff = cascade.effective.clone();
        if input_too_long(&eff, content) {
            return Err(DomainError::out_of_range(Res::Chat, "content", "INPUT_TOO_LONG", "message exceeds the model input limit"));
        }
        if image_count > 0 {
            if ks.disable_images {
                return Err(DomainError::feature_disabled("images"));
            }
            if !eff.supports_vision() {
                return Err(DomainError::invalid(Res::Chat, "content_type", "VISION_NOT_SUPPORTED", "model does not support image input"));
            }
        }

        // Context.
        let mut summary = thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat.id))
            .secure()
            .scope_with(&scopes.tenant)
            .one(&conn)
            .await?;
        if let (Some(s), Some(replaced)) = (&summary, exclude_request_id) {
            // Retry/edit: a summary covering the replaced turn is invalidated by the
            // mutation commit, so the new request must not use it either.
            let replaced_user = messages::Entity::find()
                .filter(
                    Condition::all()
                        .add(messages::Column::ChatId.eq(chat.id))
                        .add(messages::Column::RequestId.eq(replaced))
                        .add(messages::Column::Role.eq("user")),
                )
                .secure()
                .scope_with(&scopes.tenant)
                .one(&conn)
                .await?;
            let covers = replaced_user
                .is_none_or(|m| (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (m.created_at, m.id));
            if covers {
                summary = None;
            }
        }
        let history = self.recent_history(&conn, scopes, chat.id, summary.as_ref(), exclude_request_id).await?;
        let mut guards = Vec::new();
        let vector_store = if gates.file_search {
            chat_vector_stores::Entity::find()
                .filter(chat_vector_stores::Column::ChatId.eq(chat.id))
                .secure()
                .scope_with(&scopes.tenant)
                .one(&conn)
                .await?
                .and_then(|v| v.vector_store_id)
        } else {
            None
        };
        let file_search = gates.file_search && vector_store.is_some();
        if file_search {
            guards.push(self.cfg.context.file_search_guard.clone());
        }
        if gates.web_search {
            guards.push(self.cfg.context.web_search_guard.clone());
        }
        let plan = assemble(ContextInput {
            model: &eff,
            max_output_tokens_applied: cascade.reserve.max_output_tokens_applied,
            guards,
            summary: summary.as_ref().map(|s| s.summary_text.clone()),
            history,
            user_message: content,
            image_count,
            gates,
        })?;

        // Provider.
        let target = self
            .llm
            .resolver
            .chat_target(&eff.provider_id, ctx.subject_tenant_id())
            .map_err(|e| DomainError::internal(format!("provider resolution: {e}")))?;

        let mut tools = Vec::new();
        if let (true, Some(vs)) = (file_search, vector_store) {
            tools.push(ToolSpec::FileSearch { vector_store_ids: vec![vs], max_num_results: eff.max_num_results });
        }
        if gates.web_search {
            tools.push(ToolSpec::WebSearch { search_context_size: eff.web_search_context_size.clone() });
        }
        if gates.code_interpreter {
            tools.push(ToolSpec::CodeInterpreter { file_ids: ci_files.clone() });
        }
        let mut input = Vec::new();
        if let Some(s) = &plan.summary_message {
            input.push(InputMessage::text(Role::User, s.clone()));
        }
        for h in &plan.kept {
            let role = if h.role == HistoryRole::User { Role::User } else { Role::Assistant };
            input.push(InputMessage::text(role, h.content.clone()));
        }
        let mut parts = vec![ContentPart::Text(content.to_owned())];
        for img in &images {
            if let Some(fid) = &img.provider_file_id {
                parts.push(ContentPart::Image { file_id: fid.clone() });
            }
        }
        input.push(InputMessage { role: Role::User, parts });
        let feature = if tools.is_empty() {
            "none".to_owned()
        } else {
            tools.iter().map(ToolSpec::name).collect::<Vec<_>>().join("+")
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), json!(ctx.subject_tenant_id().to_string()));
        metadata.insert("user_id".into(), json!(ctx.subject_id().to_string()));
        metadata.insert("chat_id".into(), json!(chat.id.to_string()));
        metadata.insert("request_type".into(), json!("chat"));
        metadata.insert("feature".into(), json!(feature));
        let request = ChatRequest {
            model: if eff.provider_model_id.is_empty() { eff.id.clone() } else { eff.provider_model_id.clone() },
            instructions: plan.instructions.clone(),
            input,
            max_output_tokens: cascade.reserve.max_output_tokens_applied,
            max_tool_calls: Some(eff.max_tool_calls),
            tools,
            user: format!("{}{}", simple(ctx.subject_tenant_id()), simple(ctx.subject_id())),
            metadata,
            api_params: eff.general_config.api_params.clone(),
            stream: true,
        };
        let file_map = if file_search {
            chat_atts
                .iter()
                .filter_map(|a| a.provider_file_id.clone().map(|f| (f, (a.id, a.filename.clone()))))
                .collect()
        } else {
            HashMap::new()
        };
        let floor_applied = self.cfg.estimation_budgets.minimal_generation_floor.min(cascade.reserve.max_output_tokens_applied);
        let _ = request_id;
        let summary_exists = summary.is_some();
        let summary_tokens = plan.summary_message.as_ref().and(summary.as_ref()).map(|s| i64::from(s.token_estimate));
        Ok(TurnPlan {
            snapshot,
            limits,
            cascade,
            context: plan,
            target,
            request,
            file_map,
            floor_applied,
            summary_exists,
            summary_tokens,
            started_at,
        })
    }

    /// Recent uncompressed history after the summary frontier (chronological).
    pub(crate) async fn recent_history(
        &self,
        runner: &impl DBRunner,
        scopes: &ChatScopes,
        chat_id: Uuid,
        summary: Option<&thread_summaries::Model>,
        exclude_request_id: Option<Uuid>,
    ) -> DomainResult<Vec<HistoryMessage>> {
        let k = u64::from(self.cfg.context.recent_messages_limit);
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut cond = Condition::all()
            .add(messages::Column::ChatId.eq(chat_id))
            .add(messages::Column::RequestId.is_not_null())
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false))
            .add(messages::Column::Role.is_in(["user", "assistant"]));
        if let Some(rid) = exclude_request_id {
            cond = cond.add(messages::Column::RequestId.ne(rid));
        }
        if let Some(s) = summary {
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
        let mut rows = messages::Entity::find()
            .filter(cond)
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(messages::Column::Id, sea_orm::Order::Desc)
            .limit(k)
            .secure()
            .scope_with(&scopes.tenant)
            .all(runner)
            .await?;
        rows.reverse();
        Ok(rows
            .into_iter()
            .map(|m| HistoryMessage {
                id: m.id,
                role: if m.role == "assistant" { HistoryRole::Assistant } else { HistoryRole::User },
                content: m.content,
                created_at: m.created_at,
            })
            .collect())
    }

    /// `POST /v1/chats/{id}/messages:stream`.
    ///
    /// # Errors
    /// Every pre-stream rejection (JSON `Problem`).
    pub async fn send_message(self: &Arc<Self>, ctx: &SecurityContext, chat_id: Uuid, input: SendInput) -> DomainResult<TurnStream> {
        if input.content.trim().is_empty() {
            return Err(empty_content());
        }
        self.check_attachment_list(&input.attachment_ids)?;
        let scopes = self.scopes(ctx, actions::SEND_MESSAGE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = self.load_chat(&conn, &scopes, chat_id).await?;

        // 1. Idempotency (highest priority).
        if let Some(rid) = input.request_id
            && let Some(turn) = self.find_turn_any(&conn, &scopes, chat_id, rid).await?
        {
            if turn.state == "completed" && turn.deleted_at.is_none() {
                return self.replay(&conn, &scopes, &chat, &turn).await;
            }
            return Err(DomainError::aborted(Res::Turn, "request_id_conflict", "request_id was already used for another turn"));
        }
        // 2. Parallel turn guard.
        if self.running_turn(&conn, &scopes, chat_id).await?.is_some() {
            return Err(DomainError::aborted(Res::Chat, "turn_already_running", "a generation is already running for this chat"));
        }
        let request_id = input.request_id.unwrap_or_else(Uuid::new_v4);
        let attached = self
            .load_message_attachments(&conn, &scopes, chat_id, ctx.subject_id(), &input.attachment_ids, true)
            .await?;
        let plan = self
            .plan_turn(ctx, &scopes, &chat, &input.content, &attached, input.web_search, None, request_id)
            .await?;

        // Reserve transaction: quota reserve + re-check, user message, links, running turn.
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let tier = plan.cascade.effective.tier;
        let reserve = plan.cascade.reserve;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let limits = plan.limits.clone();
        let started_at = plan.started_at;
        let effective_model = plan.cascade.effective.id.clone();
        let policy_version = plan.snapshot.policy_version;
        let floor = plan.floor_applied;
        let content = input.content.clone();
        let att_ids: Vec<Uuid> = attached.iter().map(|a| a.id).collect();
        let tx_scopes = scopes.clone();
        let web_search = input.web_search;
        let res = self
            .tx(move |tx| {
                let limits = limits.clone();
                let content = content.clone();
                let att_ids = att_ids.clone();
                let scopes = tx_scopes.clone();
                let effective_model = effective_model.clone();
                Box::pin(async move {
                    reserve_and_recheck(tx, tenant_id, user_id, started_at, tier, reserve.reserved_credits_micro, &limits).await?;
                    insert_user_message(tx, &scopes, tenant_id, chat_id, user_message_id, request_id, &content, started_at, &att_ids).await?;
                    touch_chat(tx, &scopes, chat_id).await?;
                    let am = chat_turns::ActiveModel {
                        id: Set(turn_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(request_id),
                        requester_type: Set("user".into()),
                        requester_user_id: Set(Some(user_id)),
                        state: Set("running".into()),
                        provider_name: Set(None),
                        provider_response_id: Set(None),
                        assistant_message_id: Set(None),
                        error_code: Set(None),
                        reserve_tokens: Set(Some(reserve.reserve_tokens)),
                        max_output_tokens_applied: Set(Some(i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        reserved_credits_micro: Set(Some(reserve.reserved_credits_micro)),
                        policy_version_applied: Set(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))),
                        effective_model: Set(Some(effective_model)),
                        minimal_generation_floor_applied: Set(Some(i32::try_from(floor).unwrap_or(i32::MAX))),
                        error_detail: Set(None),
                        deleted_at: Set(None),
                        replaced_by_request_id: Set(None),
                        started_at: Set(started_at),
                        last_progress_at: Set(Some(started_at)),
                        web_search_enabled: Set(web_search),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(started_at),
                    };
                    chat_turns::Entity::insert(am)
                        .secure()
                        .scope_unchecked(&scopes.tenant)?
                        .exec(tx)
                        .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = res {
            if e.is_unique_violation() {
                let conn = self.db.conn()?;
                if self.find_turn_any(&conn, &scopes, chat_id, request_id).await?.is_some() {
                    return Err(DomainError::aborted(Res::Turn, "request_id_conflict", "request_id was already used for another turn"));
                }
                return Err(DomainError::aborted(Res::Chat, "turn_already_running", "a generation is already running for this chat"));
            }
            return Err(e);
        }
        Ok(self.spawn_turn(Self::live_turn(ctx, &chat, turn_id, request_id, plan)))
    }

    pub(crate) fn live_turn(ctx: &SecurityContext, chat: &chats::Model, turn_id: Uuid, request_id: Uuid, plan: TurnPlan) -> LiveTurn {
        let eff = plan.cascade.effective.clone();
        LiveTurn {
            turn_id,
            request_id,
            chat_id: chat.id,
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            assistant_message_id: Uuid::new_v4(),
            selected_model: chat.model.clone(),
            effective_model: eff.id.clone(),
            tier: eff.tier,
            decision: plan.cascade.decision,
            downgrade_reason: plan.cascade.downgrade_reason.clone(),
            reserve: ReserveFields {
                reserve_tokens: plan.cascade.reserve.reserve_tokens,
                max_output_tokens_applied: i64::from(plan.cascade.reserve.max_output_tokens_applied),
                reserved_credits_micro: plan.cascade.reserve.reserved_credits_micro,
                minimal_generation_floor_applied: i64::from(plan.floor_applied),
            },
            policy_version: plan.snapshot.policy_version,
            started_at: plan.started_at,
            target: plan.target,
            request: plan.request,
            file_map: plan.file_map,
            thread_summary_applied: plan.summary_tokens,
            context_truncated: plan.context.messages_truncated,
            assembled_tokens: plan.context.assembled_tokens,
            effective_budget: plan.context.effective_budget,
            summary_exists: plan.summary_exists,
            limits: plan.limits,
            in_mult: eff.input_tokens_credit_multiplier_micro,
            out_mult: eff.output_tokens_credit_multiplier_micro,
        }
    }

    pub(crate) async fn find_turn_any(&self, runner: &impl DBRunner, scopes: &ChatScopes, chat_id: Uuid, request_id: Uuid) -> DomainResult<Option<chat_turns::Model>> {
        Ok(chat_turns::Entity::find()
            .filter(Condition::all().add(chat_turns::Column::ChatId.eq(chat_id)).add(chat_turns::Column::RequestId.eq(request_id)))
            .secure()
            .scope_with(&scopes.tenant)
            .one(runner)
            .await?)
    }

    pub(crate) async fn running_turn(&self, runner: &impl DBRunner, scopes: &ChatScopes, chat_id: Uuid) -> DomainResult<Option<chat_turns::Model>> {
        Ok(chat_turns::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turns::Column::ChatId.eq(chat_id))
                    .add(chat_turns::Column::State.eq("running"))
                    .add(chat_turns::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scopes.tenant)
            .one(runner)
            .await?)
    }

    /// Side-effect-free replay of a completed turn.
    async fn replay(&self, runner: &impl DBRunner, scopes: &ChatScopes, chat: &chats::Model, turn: &chat_turns::Model) -> DomainResult<TurnStream> {
        let msg = match turn.assistant_message_id {
            Some(mid) => messages::Entity::find()
                .filter(Condition::all().add(messages::Column::Id.eq(mid)).add(messages::Column::ChatId.eq(chat.id)))
                .secure()
                .scope_with(&scopes.tenant)
                .one(runner)
                .await?,
            None => None,
        }
        .ok_or_else(|| DomainError::internal("completed turn without assistant message"))?;
        let effective = msg.model.clone().or_else(|| turn.effective_model.clone()).unwrap_or_else(|| chat.model.clone());
        let downgraded = effective != chat.model;
        let events = vec![
            SseEvent::StreamStarted { request_id: turn.request_id, message_id: msg.id, is_new_turn: false, thread_summary_applied: None },
            SseEvent::Delta { kind: "text", content: msg.content.clone() },
            SseEvent::Done(DonePayload {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
                effective_model: effective,
                selected_model: chat.model.clone(),
                quota_decision: if downgraded { "downgrade" } else { "allow" },
                downgrade_from: downgraded.then(|| chat.model.clone()),
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ];
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for e in events {
            drop(tx.try_send(e));
        }
        Ok(TurnStream { rx, guard: CancelOnDrop(CancellationToken::new()) })
    }

    /// Spawn the provider relay task for a running turn.
    pub(crate) fn spawn_turn(self: &Arc<Self>, live: LiveTurn) -> TurnStream {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let svc = Arc::clone(self);
        let task_cancel = cancel.clone();
        tokio::spawn(async move { svc.run_turn(live, tx, task_cancel).await });
        TurnStream { rx, guard: CancelOnDrop(cancel) }
    }

    async fn set_progress(&self, turn_id: Uuid, tenant_id: Uuid) {
        let scope = toolkit_security::AccessScope::for_tenant(tenant_id);
        if let Ok(conn) = self.db.conn() {
            drop(
                chat_turns::Entity::update_many()
                    .col_expr(chat_turns::Column::LastProgressAt, Expr::value(now()))
                    .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)).add(chat_turns::Column::State.eq("running")))
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await,
            );
        }
    }

    async fn bump_file_search_count(&self, turn_id: Uuid, tenant_id: Uuid) {
        use sea_orm::sea_query::ExprTrait as _;
        let scope = toolkit_security::AccessScope::for_tenant(tenant_id);
        if let Ok(conn) = self.db.conn() {
            drop(
                chat_turns::Entity::update_many()
                    .col_expr(
                        chat_turns::Column::FileSearchCompletedCount,
                        Expr::col(chat_turns::Column::FileSearchCompletedCount).add(1),
                    )
                    .filter(Condition::all().add(chat_turns::Column::Id.eq(turn_id)).add(chat_turns::Column::State.eq("running")))
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await,
            );
        }
    }

    /// Relay provider events to the SSE channel and finalize the turn.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // event relay loop handling every provider event kind
    async fn run_turn(self: Arc<Self>, live: LiveTurn, tx: mpsc::Sender<SseEvent>, cancel: CancellationToken) {
        let begun = Instant::now();
        let started = SseEvent::StreamStarted {
            request_id: live.request_id,
            message_id: live.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: live.thread_summary_applied,
        };
        let mut counters = ToolCounters::default();
        let mut text = String::new();
        if tx.send(started).await.is_err() {
            self.finish(&live, Outcome::Cancelled { text }, &counters, None, begun, &tx).await;
            return;
        }
        let stream: ProviderStream = tokio::select! {
            () = cancel.cancelled() => {
                self.finish(&live, Outcome::Cancelled { text }, &counters, None, begun, &tx).await;
                return;
            }
            r = self.llm.stream_chat(&live.target, &live.request) => match r {
                Ok(s) => s,
                Err(e) => {
                    let outcome = Outcome::Failed { code: e.code.as_str().to_owned(), message: e.message, usage: None };
                    self.finish(&live, outcome, &counters, None, begun, &tx).await;
                    return;
                }
            }
        };
        let mut stream = stream;
        let ping_every = Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds));
        let mut content_started = false;
        let mut last_event = tokio::time::Instant::now();
        let mut last_progress = Instant::now();
        let mut ttft: Option<Duration> = None;
        let mut annotations = Vec::new();
        loop {
            let ping_at = last_event + ping_every;
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                () = tokio::time::sleep_until(ping_at), if !content_started => {
                    last_event = tokio::time::Instant::now();
                    if tx.send(SseEvent::Ping).await.is_err() {
                        None
                    } else {
                        continue;
                    }
                }
                ev = stream.next() => Some(ev),
            };
            let Some(ev) = next else {
                drop(stream);
                self.finish(&live, Outcome::Cancelled { text }, &counters, ttft, begun, &tx).await;
                return;
            };
            last_event = tokio::time::Instant::now();
            let Some(ev) = ev else {
                drop(stream);
                let outcome = Outcome::Failed {
                    code: StreamErrorCode::ProviderError.as_str().to_owned(),
                    message: "Provider stream ended unexpectedly".to_owned(),
                    usage: None,
                };
                self.finish(&live, outcome, &counters, ttft, begun, &tx).await;
                return;
            };
            let out: Option<SseEvent> = match ev {
                ProviderEvent::TextDelta(t) => {
                    ttft.get_or_insert_with(|| begun.elapsed());
                    content_started = true;
                    text.push_str(&t);
                    Some(SseEvent::Delta { kind: "text", content: t })
                }
                ProviderEvent::ReasoningDelta(t) => {
                    content_started = true;
                    Some(SseEvent::Delta { kind: "reasoning", content: t })
                }
                ProviderEvent::ToolStart { name, details } => {
                    content_started = true;
                    let limit_code = match name.as_str() {
                        "web_search" => {
                            counters.web_search_started += 1;
                            (counters.web_search_started > self.cfg.quota.web_search_max_calls_per_message)
                                .then_some("web_search_calls_exceeded")
                        }
                        "code_interpreter" => {
                            counters.code_interpreter_started += 1;
                            (counters.code_interpreter_started > self.cfg.quota.code_interpreter_max_calls_per_message)
                                .then_some("code_interpreter_calls_exceeded")
                        }
                        _ => None,
                    };
                    if let Some(code) = limit_code {
                        drop(stream);
                        let outcome = Outcome::Failed {
                            code: code.to_owned(),
                            message: format!("Per-message tool call limit exceeded ({code})"),
                            usage: None,
                        };
                        self.finish(&live, outcome, &counters, ttft, begun, &tx).await;
                        return;
                    }
                    Some(SseEvent::Tool { phase: "start", name, details })
                }
                ProviderEvent::ToolDone { name, details } => {
                    content_started = true;
                    match name.as_str() {
                        "web_search" => counters.web_search_done += 1,
                        "code_interpreter" => counters.code_interpreter_done += 1,
                        "file_search" => {
                            counters.file_search_done += 1;
                            self.bump_file_search_count(live.turn_id, live.tenant_id).await;
                        }
                        _ => {}
                    }
                    Some(SseEvent::Tool { phase: "done", name, details })
                }
                ProviderEvent::Annotation(a) => {
                    annotations.push(a);
                    None
                }
                ProviderEvent::Completed { response_id, usage, annotations: final_anns, incomplete_reason } => {
                    drop(stream);
                    if let Some(r) = &incomplete_reason {
                        tracing::warn!(reason = %r, request_id = %live.request_id, "stream incomplete");
                    }
                    let anns = if final_anns.is_empty() { annotations } else { final_anns };
                    let citations = map_citations(&anns, &live.file_map, &text);
                    let outcome = Outcome::Completed { text, usage, response_id, citations };
                    self.finish(&live, outcome, &counters, ttft, begun, &tx).await;
                    return;
                }
                ProviderEvent::Failed { code, message, usage } => {
                    drop(stream);
                    let outcome = Outcome::Failed { code: code.as_str().to_owned(), message, usage };
                    self.finish(&live, outcome, &counters, ttft, begun, &tx).await;
                    return;
                }
            };
            if let Some(e) = out {
                if last_progress.elapsed() >= Duration::from_secs(30) {
                    last_progress = Instant::now();
                    self.set_progress(live.turn_id, live.tenant_id).await;
                }
                if tx.send(e).await.is_err() {
                    drop(stream);
                    self.finish(&live, Outcome::Cancelled { text }, &counters, ttft, begun, &tx).await;
                    return;
                }
            }
        }
    }
}

/// Map provider annotations to client citations (no provider ids exposed).
pub(crate) fn map_citations(anns: &[crate::infra::llm::types::RawAnnotation], file_map: &HashMap<String, (Uuid, String)>, _answer: &str) -> Vec<Citation> {
    use crate::infra::llm::types::RawAnnotation;
    let mut out = Vec::new();
    let mut seen_files = HashSet::new();
    for a in anns {
        match a {
            RawAnnotation::Url { url, title, start, end, part_text } => {
                let span = match (start, end) {
                    (Some(s), Some(e)) => Some((*s, *e)),
                    _ => None,
                };
                let snippet = match (span, part_text) {
                    (Some((s, e)), Some(t)) if s <= e => t.chars().skip(s).take(e - s).collect(),
                    _ => String::new(),
                };
                out.push(Citation { source: "web", title: title.clone(), url: Some(url.clone()), attachment_id: None, snippet, span });
            }
            RawAnnotation::File { file_id, .. } => {
                if let Some((aid, fname)) = file_map.get(file_id)
                    && seen_files.insert(*aid)
                {
                    out.push(Citation {
                        source: "file",
                        title: fname.clone(),
                        url: None,
                        attachment_id: Some(*aid),
                        snippet: String::new(),
                        span: None,
                    });
                }
            }
        }
    }
    out
}

/// Insert the user message and its attachment links.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_user_message(
    tx: &DbTx<'_>,
    scopes: &ChatScopes,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    request_id: Uuid,
    content: &str,
    created_at: OffsetDateTime,
    attachment_ids: &[Uuid],
) -> DomainResult<()> {
    let am = messages::ActiveModel {
        id: Set(message_id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(Some(request_id)),
        role: Set("user".into()),
        content: Set(content.to_owned()),
        content_type: Set("text".into()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".into()),
        features_used: Set(json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(created_at),
        deleted_at: Set(None),
    };
    secure_insert::<messages::Entity>(am, &scopes.tenant, tx).await?;
    for aid in attachment_ids {
        let link = message_attachments::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(*aid),
            created_at: Set(created_at),
        };
        secure_insert::<message_attachments::Entity>(link, &scopes.tenant, tx).await?;
    }
    Ok(())
}

/// Bump `chats.updated_at`.
pub(crate) async fn touch_chat(tx: &DbTx<'_>, scopes: &ChatScopes, chat_id: Uuid) -> DomainResult<()> {
    chats::Entity::update_many()
        .col_expr(chats::Column::UpdatedAt, Expr::value(now()))
        .filter(chats::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&scopes.owner)
        .exec(tx)
        .await?;
    Ok(())
}
