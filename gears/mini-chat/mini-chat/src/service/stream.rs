//! Send-message pipeline: validation, idempotency / replay, parallel-turn
//! guard, quota preflight, context assembly, reserve transaction and the
//! launch of the provider task.

use std::collections::HashMap;
use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot, UserLimits};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, Set};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use super::quota::{Preflight, reserve_in_tx};
use crate::domain::authz::{self, actions};
use crate::domain::context::{self, ContextInput, ContextPlan, HistoryMessage, Role};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::domain::estimate::estimate_text_tokens;
use crate::domain::quota::{PeriodStarts, RequestFacts, ReserveFields, ToolPlan};
use crate::infra::db::entity::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, thread_summary,
};
use crate::infra::llm::{
    ChatMessage, ChatRequest, RequestMetadata, ResolvedProvider, ToolsSpec, provider_user_field,
};
use crate::infra::repo::{self, now_utc};

// ---------------------------------------------------------------------------
// Stream events (public SSE contract, transport-agnostic)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CitationOut {
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWarningOut {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DoneOut {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub downgrade: bool,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<QuotaWarningOut>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        thread_summary_applied: Option<i32>,
    },
    Delta {
        kind: &'static str,
        content: String,
    },
    Tool {
        phase: &'static str,
        name: String,
        details: Value,
    },
    Citations(Vec<CitationOut>),
    Done(DoneOut),
    Error {
        code: String,
        message: String,
    },
}

impl StreamEvent {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }
}

/// Result of stream setup.
pub enum StreamStart {
    /// Idempotent replay of a completed turn.
    Replay(Vec<StreamEvent>),
    /// Live generation.
    Live {
        started: StreamEvent,
        rx: mpsc::Receiver<StreamEvent>,
        cancel: CancellationToken,
    },
}

// ---------------------------------------------------------------------------
// Request / plan types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Knowledge search parameters of a turn.
#[derive(Debug, Clone)]
pub struct KnowledgeParams {
    pub storage: crate::infra::llm::ResolvedStorage,
    pub vector_store_id: String,
    pub max_calls: u32,
    pub top_k: usize,
    pub max_chunk_chars: usize,
}

/// Everything the provider task and the finalizer need.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "independent feature flags")]
pub struct TurnPlan {
    pub ctx: SecurityContext,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub user_message_id: Uuid,
    pub assistant_message_id: Uuid,
    pub selected_model: String,
    pub premium: bool,
    pub downgrade: bool,
    pub downgrade_reason: Option<String>,
    pub reserve: ReserveFields,
    pub policy_version: u64,
    pub periods: PeriodStarts,
    pub limits: UserLimits,
    pub effective: ModelCatalogEntry,
    pub provider: ResolvedProvider,
    pub request: ChatRequest,
    pub citation_map: HashMap<String, (Uuid, String)>,
    pub summary_token_estimate: Option<i32>,
    pub summary_exists: bool,
    pub assembled_tokens: i64,
    pub messages_truncated: bool,
    pub knowledge: Option<KnowledgeParams>,
    pub web_search_requested: bool,
    pub started: std::time::Instant,
}

/// Inputs shared by send and retry/edit setup.
pub struct SetupInputs<'a> {
    pub ctx: &'a SecurityContext,
    pub chat: &'a chat::Model,
    pub tenant_scope: &'a AccessScope,
    pub snapshot: PolicySnapshot,
    pub content: &'a str,
    /// Attachments referenced by the user message (images become inputs).
    pub message_attachments: Vec<attachment::Model>,
    pub web_search: bool,
    pub request_id: Uuid,
    pub turn_id: Uuid,
    /// Exclude messages of these request ids from the history (the turn
    /// being replaced on retry/edit).
    pub exclude_request_ids: Vec<Uuid>,
}

/// Outcome of the read-only turn guards.
pub struct TurnGuards {
    pub boundary: Option<(OffsetDateTime, Uuid)>,
    pub facts_att: ChatAttachmentFacts,
    pub num_images: u32,
    pub pf: Preflight,
}

/// Facts about the chat's attachments.
#[derive(Debug, Clone, Default)]
pub struct ChatAttachmentFacts {
    pub ready: Vec<attachment::Model>,
    pub vector_store_id: Option<String>,
}

impl ChatAttachmentFacts {
    #[must_use]
    pub fn has_ready_docs(&self) -> bool {
        self.ready
            .iter()
            .any(|a| a.attachment_kind == "document" && a.for_file_search)
    }

    #[must_use]
    pub fn has_ready_ci(&self) -> bool {
        self.ready
            .iter()
            .any(|a| a.attachment_kind == "document" && a.for_code_interpreter)
    }
}

/// Validate content (non-empty after trim).
///
/// # Errors
/// 400 `EMPTY_CONTENT`.
pub fn validate_content(content: &str) -> DomainResult<()> {
    if content.trim().is_empty() {
        return Err(DomainError::invalid(
            Res::Chat,
            "content",
            "EMPTY_CONTENT",
            "content must not be empty",
        ));
    }
    Ok(())
}

fn tuple_le(created: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.lt(created))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(created))
                .add(message::Column::Id.lte(id)),
        )
}

fn tuple_gt(created: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.gt(created))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(created))
                .add(message::Column::Id.gt(id)),
        )
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn chat_attachment_facts(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<ChatAttachmentFacts> {
    let ready = repo::ready_attachments(r, scope, chat_id).await?;
    let vs = chat_vector_store::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
        .one(r)
        .await?;
    Ok(ChatAttachmentFacts {
        ready,
        vector_store_id: vs.and_then(|v| v.vector_store_id),
    })
}

/// `input_tokens + output_tokens` of the most recent assistant message with
/// non-zero usage.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn prior_context_tokens(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<i64> {
    let m = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(message::Column::Role.eq("assistant"))
                .add(
                    Condition::any()
                        .add(message::Column::InputTokens.gt(0))
                        .add(message::Column::OutputTokens.gt(0)),
                ),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(r)
        .await?;
    Ok(m.map_or(0, |m| m.input_tokens.saturating_add(m.output_tokens)))
}

/// Recent messages (chronological) for context assembly.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn recent_messages(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    boundary: Option<(OffsetDateTime, Uuid)>,
    frontier: Option<(OffsetDateTime, Uuid)>,
    exclude_request_ids: &[Uuid],
    limit: u32,
) -> DomainResult<Vec<message::Model>> {
    if limit == 0 {
        return Ok(vec![]);
    }
    let Some((bc, bid)) = boundary else {
        return Ok(vec![]);
    };
    let mut cond = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::RequestId.is_not_null())
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(tuple_le(bc, bid));
    if let Some((fc, fid)) = frontier {
        cond = cond.add(tuple_gt(fc, fid));
    }
    if !exclude_request_ids.is_empty() {
        cond = cond.add(message::Column::RequestId.is_not_in(exclude_request_ids.to_vec()));
    }
    let mut rows = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(cond)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(u64::from(limit))
        .all(r)
        .await?;
    rows.reverse();
    Ok(rows)
}

impl AppState {
    /// Resolve the chat's model in the current snapshot (without the enabled
    /// filter).
    ///
    /// # Errors
    /// 400 `INVALID_MODEL` when the model is no longer in the catalog.
    pub async fn chat_model_snapshot(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
    ) -> DomainResult<PolicySnapshot> {
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        if snapshot.find(&chat.model).is_none() {
            return Err(DomainError::invalid_model(format!(
                "the chat's model '{}' is no longer available",
                chat.model
            )));
        }
        Ok(snapshot)
    }

    /// Read-only guards shared by send and the retry/edit preview: kill
    /// switches, image count, quota cascade (with daily tool quotas), input
    /// limit and image guards.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn turn_guards(&self, inp: &SetupInputs<'_>) -> DomainResult<TurnGuards> {
        let ctx = inp.ctx;
        let chat = inp.chat;
        let scope = inp.tenant_scope;
        let ks = inp.snapshot.kill_switches;
        let conn = self.conn()?;
        let boundary = repo::latest_message(&conn, scope, chat.id)
            .await?
            .map(|m| (m.created_at, m.id));
        let prior = prior_context_tokens(&conn, scope, chat.id).await?;
        let facts_att = chat_attachment_facts(&conn, scope, chat.id).await?;
        let num_images = u32::try_from(
            inp.message_attachments
                .iter()
                .filter(|a| a.attachment_kind == "image")
                .count(),
        )
        .unwrap_or(u32::MAX);

        if num_images > self.cfg.rag.max_images_per_message {
            return Err(DomainError::out_of_range(
                Res::Chat,
                "image_count",
                "TOO_MANY_IMAGES",
                format!(
                    "at most {} images per message are allowed",
                    self.cfg.rag.max_images_per_message
                ),
            ));
        }
        if inp.web_search && ks.disable_web_search {
            return Err(DomainError::feature_disabled("web_search"));
        }
        let facts = RequestFacts {
            message_bytes: inp.content.len(),
            prior_context_tokens: prior,
            num_images,
            has_ready_docs: facts_att.has_ready_docs(),
            has_ready_ci: facts_att.has_ready_ci(),
            web_search_requested: inp.web_search,
        };
        let pf: Preflight = self
            .quota_preflight(ctx, inp.snapshot.clone(), &chat.model, &facts)
            .await?;
        let effective = &pf.decision.effective;
        let budgets = &effective.estimation_budgets;
        // Input limit of the current message.
        if effective.max_input_tokens > 0 {
            let est = estimate_text_tokens(inp.content.len(), budgets);
            if est > i64::from(effective.max_input_tokens) {
                return Err(DomainError::out_of_range(
                    Res::Chat,
                    "content",
                    "INPUT_TOO_LONG",
                    "the message exceeds the model's input token limit",
                ));
            }
        }
        if num_images > 0 {
            if ks.disable_images {
                return Err(DomainError::feature_disabled("images"));
            }
            if !effective.supports_vision() {
                return Err(DomainError::invalid(
                    Res::Chat,
                    "content_type",
                    "VISION_NOT_SUPPORTED",
                    "the model used for this turn does not accept image input",
                ));
            }
        }
        Ok(TurnGuards {
            boundary,
            facts_att,
            num_images,
            pf,
        })
    }

    /// Shared preflight for send and retry/edit: guards, context assembly
    /// and provider resolution. Performs no writes.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    #[allow(
        clippy::too_many_lines,
        reason = "linear preflight pipeline kept in one place"
    )]
    pub async fn plan_turn(&self, inp: SetupInputs<'_>) -> DomainResult<TurnPlan> {
        let TurnGuards {
            boundary,
            facts_att,
            num_images,
            pf,
        } = self.turn_guards(&inp).await?;
        let ctx = inp.ctx;
        let chat = inp.chat;
        let scope = inp.tenant_scope;
        let conn = self.conn()?;
        let images: Vec<&attachment::Model> = inp
            .message_attachments
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .collect();
        let effective = pf.decision.effective.clone();
        let budgets = effective.estimation_budgets.clone();
        let tools: ToolPlan = pf.decision.reserve.tools;
        let reserve = &pf.decision.reserve;

        // Tool specs.
        let mut spec = ToolsSpec {
            web_search_max_uses: self.cfg.quota.web_search_max_calls_per_message,
            ..ToolsSpec::default()
        };
        let file_search_on = match (&facts_att.vector_store_id, tools.file_search) {
            (Some(vs), true) => {
                spec.file_search = Some((vs.clone(), effective.max_num_results.max(1)));
                true
            }
            _ => false,
        };
        if tools.web_search {
            spec.web_search = Some(effective.web_search_context_size.clone());
        }
        if tools.code_interpreter {
            let files: Vec<String> = facts_att
                .ready
                .iter()
                .filter(|a| a.for_code_interpreter)
                .filter_map(|a| a.provider_file_id.clone())
                .collect();
            if !files.is_empty() {
                spec.code_interpreter = Some(files);
            }
        }
        let knowledge = if file_search_on {
            None
        } else {
            self.knowledge_params(ctx)
        };
        spec.knowledge_search = knowledge.is_some();

        // Instructions with tool guards.
        let mut instructions = effective.system_prompt.clone();
        let mut push_guard = |g: &str| {
            if g.trim().is_empty() {
                return;
            }
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(g);
        };
        if tools.file_search {
            push_guard(&self.cfg.context.file_search_guard);
        }
        if tools.web_search {
            push_guard(&self.cfg.context.web_search_guard);
        }
        if knowledge.is_some() {
            push_guard(&self.cfg.knowledge_search.guard);
        }

        // Context assembly.
        let summary = repo::find_summary(&conn, scope, chat.id).await?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let recent = recent_messages(
            &conn,
            scope,
            chat.id,
            boundary,
            frontier,
            &inp.exclude_request_ids,
            self.cfg.context.recent_messages_limit,
        )
        .await?;
        let max_out = reserve.max_output_tokens_applied;
        let Some(input_limit) = context::input_limit(
            effective.context_window,
            effective.max_input_tokens,
            max_out,
        ) else {
            return Err(context_budget_error());
        };
        let token_budget =
            input_limit - tools.surcharge(&effective) - i64::from(budgets.fixed_overhead_tokens);
        let plan: ContextPlan = context::assemble(&ContextInput {
            instructions: instructions.clone(),
            summary_text: summary.as_ref().map(|s| s.summary_text.clone()),
            recent: recent
                .iter()
                .filter(|m| m.role == "user" || m.role == "assistant")
                .map(|m| HistoryMessage {
                    role: if m.role == "assistant" {
                        Role::Assistant
                    } else {
                        Role::User
                    },
                    content: m.content.clone(),
                })
                .collect(),
            user_message: inp.content.to_owned(),
            num_images,
            budgets: &budgets,
            token_budget,
        })
        .map_err(|_| context_budget_error())?;

        // Provider resolution.
        let provider = self
            .llm
            .registry
            .resolve(&effective.provider_id, Some(ctx.subject_tenant_id()))
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "no provider '{}' configured for model '{}'",
                    effective.provider_id, effective.id
                ))
            })?;

        let mut messages = Vec::new();
        if let Some(s) = &plan.summary_message {
            messages.push(ChatMessage {
                role: "user",
                text: s.clone(),
                image_file_ids: vec![],
            });
        }
        for (role, text) in &plan.history {
            messages.push(ChatMessage {
                role: role.as_str(),
                text: text.clone(),
                image_file_ids: vec![],
            });
        }
        messages.push(ChatMessage {
            role: "user",
            text: inp.content.to_owned(),
            image_file_ids: images
                .iter()
                .filter_map(|a| a.provider_file_id.clone())
                .collect(),
        });

        let citation_map: HashMap<String, (Uuid, String)> = if file_search_on {
            facts_att
                .ready
                .iter()
                .filter_map(|a| {
                    a.provider_file_id
                        .clone()
                        .map(|f| (f, (a.id, a.filename.clone())))
                })
                .collect()
        } else {
            HashMap::new()
        };

        let request = ChatRequest {
            model: effective.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            messages,
            max_output_tokens: max_out,
            tools: spec,
            max_tool_calls: effective.max_tool_calls,
            api_params: effective.general_config.api_params.clone(),
            user: provider_user_field(ctx.subject_tenant_id(), ctx.subject_id()),
            metadata: RequestMetadata {
                tenant_id: ctx.subject_tenant_id().to_string(),
                user_id: ctx.subject_id().to_string(),
                chat_id: chat.id.to_string(),
                request_type: "chat",
                feature: tools.feature_label(),
            },
            stream: true,
            extra_input: vec![],
        };
        let floor = i64::from(self.cfg.estimation_budgets.minimal_generation_floor).min(max_out);
        Ok(TurnPlan {
            ctx: ctx.clone(),
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            turn_id: inp.turn_id,
            request_id: inp.request_id,
            user_message_id: Uuid::now_v7(),
            assistant_message_id: Uuid::now_v7(),
            selected_model: chat.model.clone(),
            premium: pf.effective_is_premium(),
            downgrade: pf.decision.is_downgrade,
            downgrade_reason: pf.decision.downgrade_reason.clone(),
            reserve: ReserveFields {
                reserve_tokens: reserve.reserve_tokens,
                max_output_tokens_applied: max_out,
                reserved_credits_micro: reserve.reserved_credits_micro,
                minimal_generation_floor_applied: floor,
            },
            policy_version: pf.snapshot.policy_version,
            periods: pf.periods,
            limits: pf.limits,
            effective,
            provider,
            request,
            citation_map,
            summary_token_estimate: if plan.summary_kept {
                summary.as_ref().map(|s| s.token_estimate)
            } else {
                None
            },
            summary_exists: summary.is_some(),
            assembled_tokens: plan.assembled_tokens,
            messages_truncated: plan.messages_truncated,
            knowledge,
            web_search_requested: inp.web_search,
            started: std::time::Instant::now(),
        })
    }

    fn knowledge_params(&self, ctx: &SecurityContext) -> Option<KnowledgeParams> {
        let k = &self.cfg.knowledge_search;
        if !k.enabled {
            return None;
        }
        let pid = k.provider_id.as_deref()?;
        let entry = self.llm.registry.entries.get(pid)?;
        if !matches!(
            entry.kind,
            crate::config::ProviderKind::OpenaiResponses
                | crate::config::ProviderKind::AnthropicMessages
        ) || entry.api_version.as_deref().is_none_or(str::is_empty)
        {
            tracing::warn!("knowledge search parameters cannot be built; knowledge search is off");
            return None;
        }
        let storage = self
            .llm
            .registry
            .storage_by_id(pid, Some(ctx.subject_tenant_id()))
            .or_else(|| {
                Some(crate::infra::llm::ResolvedStorage {
                    provider_id: pid.to_owned(),
                    backend_label: pid.to_owned(),
                    alias: self
                        .llm
                        .registry
                        .alias_for(entry, Some(ctx.subject_tenant_id())),
                    storage_kind: crate::config::StorageKind::Azure,
                    api_version: entry.api_version.clone(),
                })
            })?;
        Some(KnowledgeParams {
            storage,
            vector_store_id: k.vector_store_id.clone()?,
            max_calls: k.max_calls_per_message,
            top_k: k.top_k,
            max_chunk_chars: k.max_chunk_chars,
        })
    }

    /// `POST /v1/chats/{id}/messages:stream`.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn start_send(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> DomainResult<StreamStart> {
        let scopes =
            authz::chat_scopes(&self.enforcer, &ctx, actions::SEND_MESSAGE, Some(chat_id)).await?;
        let conn = self.conn()?;
        let chat = repo::find_chat(&conn, &scopes.owner, chat_id)
            .await?
            .ok_or_else(|| DomainError::not_found(Res::Chat, chat_id))?;
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        if let Some(t) =
            repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id).await?
        {
            if t.state == "completed" && t.deleted_at.is_none() {
                return Ok(StreamStart::Replay(
                    self.replay_events(&conn, &scopes.tenant, &chat, &t).await?,
                ));
            }
            return Err(DomainError::aborted(
                "request_id_conflict",
                "The request_id was already used for another turn",
            ));
        }
        if repo::find_running_turn(&conn, &scopes.tenant, chat_id)
            .await?
            .is_some()
        {
            return Err(DomainError::aborted(
                "turn_already_running",
                "A generation is already running for this chat",
            ));
        }
        // Preflight validation (after the idempotency and parallel-turn checks).
        validate_content(&req.content)?;
        let mut seen = std::collections::HashSet::new();
        for id in &req.attachment_ids {
            if !seen.insert(*id) {
                return Err(DomainError::invalid_attachment("duplicate attachment id"));
            }
        }
        let max_ids = self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message;
        if req.attachment_ids.len() > max_ids as usize {
            return Err(DomainError::invalid_attachment("too many attachment ids"));
        }
        let snapshot = self.chat_model_snapshot(&ctx, &chat).await?;
        // Referenced attachments (validated again in the reserve transaction).
        let msg_atts = if req.attachment_ids.is_empty() {
            vec![]
        } else {
            attachment::Entity::find()
                .secure()
                .scope_with(&scopes.tenant)
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(chat_id))
                        .add(attachment::Column::Id.is_in(req.attachment_ids.clone()))
                        .add(attachment::Column::DeletedAt.is_null()),
                )
                .all(&conn)
                .await?
        };
        let turn_id = Uuid::now_v7();
        let plan = self
            .plan_turn(SetupInputs {
                ctx: &ctx,
                chat: &chat,
                tenant_scope: &scopes.tenant,
                snapshot,
                content: &req.content,
                message_attachments: msg_atts,
                web_search: req.web_search,
                request_id,
                turn_id,
                exclude_request_ids: vec![],
            })
            .await?;
        self.commit_send(&scopes.tenant, &plan, &req).await?;
        Ok(self.launch(plan))
    }

    /// Reserve transaction of the send path.
    async fn commit_send(
        &self,
        scope: &AccessScope,
        plan: &TurnPlan,
        req: &SendRequest,
    ) -> DomainResult<()> {
        let plan_c = plan.clone();
        let scope_c = scope.clone();
        let content = req.content.clone();
        let att_ids = req.attachment_ids.clone();
        let web_search = req.web_search;
        let res = self
            .write_tx(move |tx| {
                let plan = plan_c.clone();
                let scope = scope_c.clone();
                let content = content.clone();
                let att_ids = att_ids.clone();
                Box::pin(async move {
                    reserve_in_tx(
                        tx,
                        &AccessScope::for_tenant(plan.tenant_id),
                        plan.tenant_id,
                        plan.user_id,
                        &plan.periods,
                        plan.premium,
                        plan.reserve.reserved_credits_micro,
                        &plan.limits,
                    )
                    .await?;
                    let created = repo::next_message_time(tx, &scope, plan.chat_id).await?;
                    insert_user_message(tx, &scope, &plan, &content, created).await?;
                    repo::touch_chat(tx, &scope, plan.chat_id, created).await?;
                    link_attachments(tx, &scope, &plan, &att_ids, created, true).await?;
                    insert_running_turn(tx, &scope, &plan, web_search, true).await?;
                    Ok(())
                })
            })
            .await;
        match res {
            Ok(()) => Ok(()),
            Err(e) if e.is_unique_violation() => {
                let conn = self.conn()?;
                if repo::find_turn_by_request(&conn, scope, plan.chat_id, plan.request_id)
                    .await?
                    .is_some()
                {
                    Err(DomainError::aborted(
                        "request_id_conflict",
                        "The request_id was already used for another turn",
                    ))
                } else {
                    Err(DomainError::aborted(
                        "turn_already_running",
                        "A generation is already running for this chat",
                    ))
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Spawn the provider task and return the live stream handles.
    #[must_use]
    pub fn launch(self: &Arc<Self>, plan: TurnPlan) -> StreamStart {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let started = StreamEvent::StreamStarted {
            request_id: plan.request_id,
            message_id: plan.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: plan.summary_token_estimate,
        };
        let state = Arc::clone(self);
        let c = cancel.clone();
        tokio::spawn(async move {
            super::provider_task::run(state, plan, tx, c).await;
        });
        StreamStart::Live {
            started,
            rx,
            cancel,
        }
    }

    /// Events of an idempotent replay.
    ///
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn replay_events(
        &self,
        r: &impl DBRunner,
        scope: &AccessScope,
        chat: &chat::Model,
        turn: &chat_turn::Model,
    ) -> DomainResult<Vec<StreamEvent>> {
        let msg = match turn.assistant_message_id {
            Some(id) => {
                message::Entity::find()
                    .secure()
                    .scope_with(scope)
                    .filter(
                        Condition::all()
                            .add(message::Column::Id.eq(id))
                            .add(message::Column::ChatId.eq(chat.id)),
                    )
                    .one(r)
                    .await?
            }
            None => None,
        }
        .ok_or_else(|| DomainError::internal("completed turn without an assistant message"))?;
        let effective = msg
            .model
            .clone()
            .or_else(|| turn.effective_model.clone())
            .unwrap_or_else(|| chat.model.clone());
        let downgrade = effective != chat.model;
        Ok(vec![
            StreamEvent::StreamStarted {
                request_id: turn.request_id,
                message_id: msg.id,
                is_new_turn: false,
                thread_summary_applied: None,
            },
            StreamEvent::Delta {
                kind: "text",
                content: msg.content.clone(),
            },
            StreamEvent::Done(DoneOut {
                input_tokens: msg.input_tokens,
                output_tokens: msg.output_tokens,
                effective_model: effective,
                selected_model: chat.model.clone(),
                downgrade,
                downgrade_from: downgrade.then(|| chat.model.clone()),
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ])
    }
}

fn context_budget_error() -> DomainError {
    DomainError::out_of_range(
        Res::Chat,
        "content",
        "CONTEXT_BUDGET_EXCEEDED",
        "the mandatory context does not fit the model's input budget",
    )
}

/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn insert_user_message(
    tx: &impl DBRunner,
    scope: &AccessScope,
    plan: &TurnPlan,
    content: &str,
    created: OffsetDateTime,
) -> DomainResult<()> {
    let am = message::ActiveModel {
        id: Set(plan.user_message_id),
        tenant_id: Set(plan.tenant_id),
        chat_id: Set(plan.chat_id),
        request_id: Set(Some(plan.request_id)),
        role: Set("user".to_owned()),
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
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(created),
        deleted_at: Set(None),
    };
    secure_insert::<message::Entity>(am, scope, tx).await?;
    Ok(())
}

/// Validate referenced attachments and link them to the user message.
/// `strict` rejects invalid ids (send); retry/edit silently skip deleted ones.
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn link_attachments(
    tx: &impl DBRunner,
    scope: &AccessScope,
    plan: &TurnPlan,
    ids: &[Uuid],
    created: OffsetDateTime,
    strict: bool,
) -> DomainResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let atts = attachment::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(attachment::Column::ChatId.eq(plan.chat_id))
                .add(attachment::Column::TenantId.eq(plan.tenant_id))
                .add(attachment::Column::Id.is_in(ids.to_vec()))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .all(tx)
        .await?;
    let by_id: HashMap<Uuid, attachment::Model> = atts.into_iter().map(|a| (a.id, a)).collect();
    for id in ids {
        let valid = by_id
            .get(id)
            .is_some_and(|a| a.uploaded_by_user_id == plan.user_id && a.status == "ready");
        if !valid {
            if strict {
                return Err(DomainError::invalid_attachment(format!(
                    "attachment {id} is unknown, not ready or not accessible"
                )));
            }
            continue;
        }
        let am = message_attachment::ActiveModel {
            tenant_id: Set(plan.tenant_id),
            chat_id: Set(plan.chat_id),
            message_id: Set(plan.user_message_id),
            attachment_id: Set(*id),
            created_at: Set(created),
        };
        secure_insert::<message_attachment::Entity>(am, scope, tx).await?;
    }
    Ok(())
}

/// Insert the `running` turn (with preflight columns when `with_reserve`).
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn insert_running_turn(
    tx: &impl DBRunner,
    scope: &AccessScope,
    plan: &TurnPlan,
    web_search: bool,
    with_reserve: bool,
) -> DomainResult<()> {
    let now = now_utc();
    let r = &plan.reserve;
    let am = chat_turn::ActiveModel {
        id: Set(plan.turn_id),
        tenant_id: Set(plan.tenant_id),
        chat_id: Set(plan.chat_id),
        request_id: Set(plan.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(plan.user_id)),
        state: Set("running".to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(with_reserve.then_some(r.reserve_tokens)),
        max_output_tokens_applied: Set(
            with_reserve.then(|| i32::try_from(r.max_output_tokens_applied).unwrap_or(i32::MAX))
        ),
        reserved_credits_micro: Set(with_reserve.then_some(r.reserved_credits_micro)),
        policy_version_applied: Set(
            with_reserve.then(|| i64::try_from(plan.policy_version).unwrap_or(i64::MAX))
        ),
        effective_model: Set(with_reserve.then(|| plan.effective.id.clone())),
        minimal_generation_floor_applied: Set(with_reserve
            .then(|| i32::try_from(r.minimal_generation_floor_applied).unwrap_or(i32::MAX))),
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
    secure_insert::<chat_turn::Entity>(am, scope, tx).await?;
    Ok(())
}

/// Thread summary row of a chat (used by mutations).
///
/// # Errors
/// Returns the domain error of the step that failed (validation,
/// authorization, persistence or a downstream dependency).
pub async fn summary_row(
    r: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> DomainResult<Option<thread_summary::Model>> {
    repo::find_summary(r, scope, chat_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_validation() {
        assert!(validate_content("  ").is_err());
        assert!(validate_content("hi").is_ok());
    }
}
