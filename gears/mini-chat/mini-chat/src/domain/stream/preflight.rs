//! Preflight: attachment validation, quota cascade, guards, context assembly
//! and provider request building (everything before the reserve).

use std::collections::{HashMap, HashSet};

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot, UserLimits};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::context::{BudgetInputs, ContextPlan, HistoryMessage, assemble, system_prompt_with_guards};
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimate::estimate_str;
use crate::domain::quota::{CascadeDecision, PeriodStarts, RequestFacts, cascade, read_usage};
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{attachments, chat_vector_stores, chats, messages, thread_summaries};
use crate::infra::llm::resolver::{KnowledgeTarget, ResolvedProvider};
use crate::infra::llm::{
    InputMessage, LlmRequest, RequestMetadata, Role, ToolSpec, feature_label, provider_user_field,
};

/// Chat state that drives tool inclusion and estimation.
#[derive(Debug, Clone, Default)]
pub struct ChatFacts {
    pub has_ready_docs: bool,
    pub vector_store_id: Option<String>,
    pub xlsx_file_ids: Vec<String>,
    /// `provider_file_id → (attachment_id, filename)` for citations.
    pub file_map: HashMap<String, (Uuid, String)>,
    pub prior_context_tokens: i64,
}

/// Result of the quota preflight.
#[derive(Debug, Clone)]
pub struct QuotaPlan {
    pub snapshot: PolicySnapshot,
    pub selected_model: String,
    pub decision: CascadeDecision,
    pub limits: UserLimits,
    pub periods: PeriodStarts,
    pub floor_applied: i64,
    pub facts: ChatFacts,
}

/// Assembled provider request.
#[derive(Debug, Clone)]
pub struct RequestPlan {
    pub context: ContextPlan,
    pub has_summary: bool,
    /// Stored `thread_summaries.token_estimate` when the summary was kept in
    /// the context (`stream_started.thread_summary_applied`).
    pub summary_token_estimate: Option<i64>,
    /// Knowledge-search target when `search_knowledge` is offered.
    pub knowledge: Option<KnowledgeTarget>,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
}

impl MiniChat {
    /// Load the chat facts (tenant-scoped child queries of an authorized chat).
    ///
    /// # Errors
    /// Database failure.
    pub async fn chat_facts(&self, runner: &impl DBRunner, chat: &chats::Model) -> Result<ChatFacts, DomainError> {
        let scope = AccessScope::for_tenant(chat.tenant_id);
        let ready = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat.id))
                    .add(attachments::Column::Status.eq("ready"))
                    .add(attachments::Column::DeletedAt.is_null()),
            )
            .order_by_asc(attachments::Column::CreatedAt)
            .secure()
            .scope_with(&scope)
            .all(runner)
            .await?;
        let mut f = ChatFacts::default();
        for a in &ready {
            if a.attachment_kind == "document" && a.for_file_search {
                f.has_ready_docs = true;
            }
            if let Some(pid) = &a.provider_file_id {
                if a.for_code_interpreter {
                    f.xlsx_file_ids.push(pid.clone());
                }
                f.file_map.insert(pid.clone(), (a.id, a.filename.clone()));
            }
        }
        if f.has_ready_docs {
            f.vector_store_id = chat_vector_stores::Entity::find()
                .filter(
                    Condition::all()
                        .add(chat_vector_stores::Column::TenantId.eq(chat.tenant_id))
                        .add(chat_vector_stores::Column::ChatId.eq(chat.id)),
                )
                .secure()
                .scope_with(&scope)
                .one(runner)
                .await?
                .and_then(|r| r.vector_store_id);
            if f.vector_store_id.is_none() {
                f.has_ready_docs = false;
            }
        }
        let prior = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat.id))
                    .add(messages::Column::Role.eq("assistant"))
                    .add(messages::Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(messages::Column::InputTokens.ne(0))
                            .add(messages::Column::OutputTokens.ne(0)),
                    ),
            )
            .order_by_desc(messages::Column::CreatedAt)
            .order_by_desc(messages::Column::Id)
            .limit(1)
            .secure()
            .scope_with(&scope)
            .one(runner)
            .await?;
        f.prior_context_tokens = prior.map_or(0, |m| m.input_tokens + m.output_tokens);
        Ok(f)
    }

    /// Validate `attachment_ids` (unique, count, same chat, uploader, ready).
    /// Returns the referenced rows in request order.
    ///
    /// # Errors
    /// `invalid_attachment` (400) or `TOO_MANY_IMAGES`.
    pub async fn validate_attachments(
        &self,
        runner: &impl DBRunner,
        ctx: &SecurityContext,
        chat: &chats::Model,
        ids: &[Uuid],
    ) -> Result<Vec<attachments::Model>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        if !ids.iter().all(|id| seen.insert(*id)) {
            return Err(DomainError::InvalidAttachment("duplicate attachment ids".into()));
        }
        let max = u64::from(self.cfg.rag.max_documents_per_chat) + u64::from(self.cfg.rag.max_images_per_message);
        if ids.len() as u64 > max {
            return Err(DomainError::InvalidAttachment("too many attachment ids".into()));
        }
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat.id))
                    .add(attachments::Column::Id.is_in(ids.to_vec())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .all(runner)
            .await?;
        let by_id: HashMap<Uuid, attachments::Model> = rows.into_iter().map(|r| (r.id, r)).collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let a = by_id
                .get(id)
                .filter(|a| {
                    a.deleted_at.is_none()
                        && a.uploaded_by_user_id == ctx.subject_id()
                        && a.tenant_id == ctx.subject_tenant_id()
                        && a.status == "ready"
                })
                .ok_or_else(|| DomainError::InvalidAttachment(format!("attachment {id} is not usable")))?;
            out.push(a.clone());
        }
        let images = out.iter().filter(|a| a.attachment_kind == "image").count();
        if images as u64 > u64::from(self.cfg.rag.max_images_per_message) {
            return Err(DomainError::TooManyImages(format!(
                "at most {} images per message",
                self.cfg.rag.max_images_per_message
            )));
        }
        Ok(out)
    }

    /// Quota preflight: kill switches, cascade, daily tool quotas, input
    /// length and image guards. Changes nothing.
    ///
    /// # Errors
    /// 400 / 429 / 500 as documented.
    pub async fn quota_preflight(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        text: &str,
        image_count: usize,
        web_search: bool,
    ) -> Result<QuotaPlan, DomainError> {
        let snapshot = self.snapshot(ctx).await?;
        Self::chat_model(&snapshot, &chat.model)?;
        let ks = snapshot.kill_switches;
        if web_search && ks.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        let conn = self.db.conn()?;
        let facts = self.chat_facts(&conn, chat).await?;
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let periods = PeriodStarts::of(crate::infra::db::now());
        let usage = read_usage(&conn, ctx.subject_tenant_id(), ctx.subject_id(), periods).await?;
        let req_facts = RequestFacts {
            message_bytes: text.len(),
            prior_context_tokens: facts.prior_context_tokens,
            image_count,
            has_ready_docs: facts.has_ready_docs,
            has_ready_xlsx: !facts.xlsx_file_ids.is_empty(),
            web_search_requested: web_search,
        };
        let decision = cascade(
            &snapshot,
            &limits,
            &usage,
            &chat.model,
            &req_facts,
            self.cfg.streaming.max_output_tokens,
        )
        .ok_or(DomainError::QuotaExceeded(QuotaScope::Tokens))?;
        if decision.reserve.tools.web_search
            && usage.daily_total.web_search_calls >= i64::from(self.cfg.quota.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::WebSearch));
        }
        if decision.reserve.tools.code_interpreter
            && usage.daily_total.code_interpreter_calls >= i64::from(self.cfg.quota.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter));
        }
        let eff = &decision.effective;
        if eff.max_input_tokens > 0 && estimate_str(text, &eff.estimation_budgets) > i64::from(eff.max_input_tokens) {
            return Err(DomainError::InputTooLong(format!(
                "the message exceeds the model input limit of {} tokens",
                eff.max_input_tokens
            )));
        }
        if image_count > 0 {
            if ks.disable_images {
                return Err(DomainError::FeatureDisabled("images"));
            }
            if !eff.supports_vision() {
                return Err(DomainError::VisionNotSupported(eff.id.clone()));
            }
        }
        let floor_applied = i64::from(self.cfg.estimation_budgets.minimal_generation_floor)
            .min(decision.reserve.max_output_tokens_applied);
        Ok(QuotaPlan {
            snapshot,
            selected_model: chat.model.clone(),
            decision,
            limits,
            periods,
            floor_applied,
            facts,
        })
    }

    /// Recent messages for context (oldest first), excluding `exclude_request_id`.
    async fn recent_messages(
        &self,
        runner: &impl DBRunner,
        chat: &chats::Model,
        summary: Option<&thread_summaries::Model>,
        exclude_request_id: Option<Uuid>,
    ) -> Result<Vec<HistoryMessage>, DomainError> {
        let limit = u64::from(self.cfg.context.recent_messages_limit);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut cond = Condition::all()
            .add(messages::Column::ChatId.eq(chat.id))
            .add(messages::Column::RequestId.is_not_null())
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false))
            .add(messages::Column::Role.ne("system"));
        if let Some(r) = exclude_request_id {
            cond = cond.add(messages::Column::RequestId.ne(r));
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
            .order_by_desc(messages::Column::CreatedAt)
            .order_by_desc(messages::Column::Id)
            .limit(limit)
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .all(runner)
            .await?;
        rows.reverse();
        Ok(rows
            .into_iter()
            .map(|m| HistoryMessage {
                role: if m.role == "assistant" { Role::Assistant } else { Role::User },
                text: m.content,
            })
            .collect())
    }

    /// Tool list and system prompt for the effective model.
    fn tools_and_prompt(&self, plan: &QuotaPlan, tenant_id: &str) -> (Vec<ToolSpec>, String, i64, Option<KnowledgeTarget>) {
        let eff = &plan.decision.effective;
        let flags = plan.decision.reserve.tools;
        let b = &eff.estimation_budgets;
        let mut tools = Vec::new();
        let mut guards: Vec<&str> = Vec::new();
        let mut surcharges = 0_i64;
        if flags.file_search
            && let Some(vs) = &plan.facts.vector_store_id
        {
            tools.push(ToolSpec::FileSearch {
                vector_store_id: vs.clone(),
                max_num_results: eff.max_num_results,
            });
            guards.push(&self.cfg.context.file_search_guard);
            surcharges += i64::from(b.tool_surcharge_tokens);
        }
        if flags.web_search {
            tools.push(ToolSpec::WebSearch {
                context_size: eff.web_search_context_size.as_str().to_owned(),
            });
            guards.push(&self.cfg.context.web_search_guard);
            surcharges += i64::from(b.web_search_surcharge_tokens);
        }
        if flags.code_interpreter {
            tools.push(ToolSpec::CodeInterpreter {
                file_ids: plan.facts.xlsx_file_ids.clone(),
            });
            surcharges += i64::from(b.code_interpreter_surcharge_tokens);
        }
        // knowledge search: never together with file_search (file_search wins)
        let knowledge = if tools.iter().any(|t| matches!(t, ToolSpec::FileSearch { .. })) {
            None
        } else {
            self.resolver.knowledge_target(&self.cfg.knowledge_search, tenant_id)
        };
        if knowledge.is_some() {
            tools.push(ToolSpec::SearchKnowledge);
            guards.push(&self.cfg.knowledge_search.guard);
        }
        (tools, system_prompt_with_guards(&eff.system_prompt, &guards), surcharges, knowledge)
    }

    /// Context assembly, provider resolution and request building.
    ///
    /// # Errors
    /// `CONTEXT_BUDGET_EXCEEDED`, provider resolution (500), database.
    pub async fn build_request(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        plan: &QuotaPlan,
        text: &str,
        images: &[attachments::Model],
        exclude_request_id: Option<Uuid>,
    ) -> Result<RequestPlan, DomainError> {
        let conn = self.db.conn()?;
        let eff: &ModelCatalogEntry = &plan.decision.effective;
        let summary = thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat.id))
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .one(&conn)
            .await?;
        let recent = self
            .recent_messages(&conn, chat, summary.as_ref(), exclude_request_id)
            .await?;
        let (tools, system_prompt, surcharges, knowledge) = self.tools_and_prompt(plan, &chat.tenant_id.to_string());
        let budget = BudgetInputs {
            context_window: i64::from(eff.context_window),
            max_input_tokens: i64::from(eff.max_input_tokens),
            max_output_tokens_applied: plan.decision.reserve.max_output_tokens_applied,
            surcharges,
            budgets: eff.estimation_budgets.clone(),
        };
        let context = assemble(
            &budget,
            &system_prompt,
            text,
            images.len(),
            summary.as_ref().map(|s| s.summary_text.as_str()),
            &recent,
        )?;
        let provider = self
            .resolver
            .resolve(&eff.provider_id, &chat.tenant_id.to_string())?;
        let mut input = context.history.clone();
        input.push(InputMessage {
            role: Role::User,
            text: text.to_owned(),
            image_file_ids: images.iter().filter_map(|a| a.provider_file_id.clone()).collect(),
            secondary_image_file_ids: images.iter().filter_map(|a| a.secondary_file_id.clone()).collect(),
        });
        let tenant = ctx.subject_tenant_id().to_string();
        let user = ctx.subject_id().to_string();
        let feature = feature_label(&tools);
        let request = LlmRequest {
            provider_model_id: eff.provider_model_id.clone(),
            instructions: system_prompt,
            input,
            max_output_tokens: u32::try_from(plan.decision.reserve.max_output_tokens_applied).unwrap_or(u32::MAX),
            tools,
            max_tool_calls: eff.max_tool_calls,
            api_params: eff.general_config.api_params.clone(),
            user: provider_user_field(&tenant, &user),
            metadata: RequestMetadata {
                tenant_id: tenant,
                user_id: user,
                chat_id: chat.id.to_string(),
                request_type: "chat".into(),
                feature,
            },
            stream: true,
            tool_exchanges: Vec::new(),
        };
        let summary_token_estimate = context
            .summary_applied
            .and(summary.as_ref().map(|s| i64::from(s.token_estimate)));
        Ok(RequestPlan {
            context,
            has_summary: summary.is_some(),
            summary_token_estimate,
            knowledge,
            provider,
            request,
        })
    }
}
