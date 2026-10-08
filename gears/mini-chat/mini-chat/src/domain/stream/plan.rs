//! Turn preflight (quota cascade, kill switches, image guards) and the turn
//! plan (context assembly, provider resolution, provider request).

use std::collections::HashMap;

use mini_chat_sdk::{PolicySnapshot, UserLimits};
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::context::{
    self, ContextInputs, ContextPlan, HistoryMessage, system_instructions,
};
use crate::domain::error::{DomainError, QuotaScope, Res};
use crate::domain::quota::{
    ChatToolState, Decision, Periods, ReserveInputs, Usage, cascade, estimate_text_tokens,
};
use crate::domain::service::Services;
use crate::infra::db::entities::{attachment, chat, thread_summary};
use crate::infra::db::repo::messages::OrderKey;
use crate::infra::db::repo::{attachments, messages, quota, summaries, vector_stores};
use crate::infra::db::tenant_scope;
use crate::infra::llm::registry::{ResolvedProvider, provider_user};
use crate::infra::llm::types::{
    ContentPart, InputItem, LlmRequest, RequestMetadata, Role, ToolSpec,
};

/// Chat attachments relevant to a turn.
#[derive(Debug, Clone, Default)]
pub struct ChatAttachments {
    pub has_ready_documents: bool,
    pub vector_store_id: Option<String>,
    pub code_interpreter_file_ids: Vec<String>,
    /// `provider_file_id → (attachment_id, filename)` for citations.
    pub file_map: HashMap<String, (Uuid, String)>,
}

/// Inputs of a turn preflight.
#[derive(Debug, Clone)]
pub struct TurnInputs {
    pub chat: chat::Model,
    pub content: String,
    /// Image attachments sent with the message.
    pub images: Vec<attachment::Model>,
    pub web_search: bool,
}

/// Result of the preflight (no writes yet).
#[derive(Debug, Clone)]
pub struct Preflight {
    pub snapshot: PolicySnapshot,
    pub limits: UserLimits,
    pub periods: Periods,
    pub decision: Decision,
    pub attachments: ChatAttachments,
    pub boundary: Option<OrderKey>,
    pub floor_applied: i64,
}

/// Knowledge search parameters of a turn.
#[derive(Debug, Clone)]
pub struct KnowledgeParams {
    pub alias: String,
    pub api_version: String,
    pub vector_store_id: String,
}

/// The executable plan of a turn.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    pub pre: Preflight,
    pub context: ContextPlan,
    pub target: ResolvedProvider,
    pub request: LlmRequest,
    pub summary: Option<thread_summary::Model>,
    pub knowledge: Option<KnowledgeParams>,
}

/// Image ids among `attachment_ids` (non-deleted attachments of the chat).
pub async fn image_attachments(
    svc: &Services,
    chat: &chat::Model,
    ids: &[Uuid],
) -> Result<Vec<attachment::Model>, DomainError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let conn = svc.db.conn()?;
    let found =
        attachments::find_many_in_chat(&conn, &tenant_scope(chat.tenant_id), chat.id, ids).await?;
    let by_id: HashMap<Uuid, attachment::Model> = found.into_iter().map(|a| (a.id, a)).collect();
    Ok(ids
        .iter()
        .filter_map(|id| by_id.get(id))
        .filter(|a| a.attachment_kind == attachments::KIND_IMAGE)
        .cloned()
        .collect())
}

impl Services {
    async fn chat_attachments(
        &self,
        scope: &AccessScope,
        chat: &chat::Model,
    ) -> Result<ChatAttachments, DomainError> {
        let conn = self.db.conn()?;
        let ready = attachments::ready_in_chat(&conn, scope, chat.id).await?;
        let vs = vector_stores::find(&conn, scope, chat.tenant_id, chat.id)
            .await?
            .and_then(|r| r.vector_store_id);
        let mut out = ChatAttachments {
            vector_store_id: vs.clone(),
            ..ChatAttachments::default()
        };
        for a in ready {
            if a.attachment_kind == attachments::KIND_DOCUMENT && a.for_file_search && vs.is_some()
            {
                out.has_ready_documents = true;
            }
            if let Some(pf) = &a.provider_file_id {
                if a.for_code_interpreter {
                    out.code_interpreter_file_ids.push(pf.clone());
                }
                out.file_map.insert(pf.clone(), (a.id, a.filename.clone()));
            }
        }
        Ok(out)
    }

    /// Preflight: snapshot boundary, attachment state, image count limit,
    /// web-search kill switch, quota cascade, daily tool quotas, input limit
    /// and image guards. Performs no writes.
    pub async fn preflight(
        &self,
        ctx: &SecurityContext,
        inp: &TurnInputs,
        snapshot: PolicySnapshot,
    ) -> Result<Preflight, DomainError> {
        let user = ctx.subject_id();
        let scope = tenant_scope(inp.chat.tenant_id);
        let conn = self.db.conn()?;
        let boundary = messages::latest_key(&conn, &scope, inp.chat.id).await?;
        let prior = messages::prior_context_tokens(&conn, &scope, inp.chat.id).await?;
        let atts = self.chat_attachments(&scope, &inp.chat).await?;

        let max_images = self.cfg.rag.max_images_per_message as usize;
        if inp.images.len() > max_images {
            return Err(DomainError::out_of_range(
                Res::Message,
                "image_count",
                "TOO_MANY_IMAGES",
                format!("At most {max_images} images per message"),
            ));
        }
        if inp.web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::feature_disabled("web_search"));
        }

        let limits = self
            .policy
            .user_limits(user, snapshot.policy_version)
            .await?;
        let periods = Periods::at(OffsetDateTime::now_utc());
        let rows = quota::rows_for_periods(
            &conn,
            &quota::user_scope(ctx.subject_tenant_id(), user),
            ctx.subject_tenant_id(),
            user,
            &periods.list(),
        )
        .await?;
        let usage = Usage::from_rows(&rows, periods);
        let reserve_inputs = ReserveInputs {
            message_bytes: inp.content.len(),
            prior_context_tokens: prior,
            image_count: inp.images.len(),
            chat: ChatToolState {
                has_ready_documents: atts.has_ready_documents,
                has_ready_code_interpreter_files: !atts.code_interpreter_file_ids.is_empty(),
                web_search_requested: inp.web_search,
            },
            max_output_tokens_cfg: self.cfg.streaming.max_output_tokens,
        };
        let decision = cascade(&inp.chat.model, &snapshot, &usage, &limits, &reserve_inputs);
        let Some(decision) = decision else {
            self.metrics.inc(
                "quota_preflight",
                &[
                    ("decision", "reject"),
                    ("model", &inp.chat.model),
                    ("tier", "none"),
                ],
            );
            return Err(DomainError::QuotaExceeded(QuotaScope::Tokens));
        };
        self.metrics.inc(
            "quota_preflight",
            &[
                ("decision", decision.decision_str()),
                ("model", &decision.effective.id),
                ("tier", decision.tier.as_str()),
            ],
        );
        let (ws_today, ci_today) = usage.daily_tool_calls();
        if decision.reserve.tools.web_search
            && ws_today >= i64::from(self.cfg.quota.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::WebSearch));
        }
        if decision.reserve.tools.code_interpreter
            && ci_today >= i64::from(self.cfg.quota.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter));
        }
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "quota_estimated_tokens",
            decision.reserve.reserve_tokens as f64,
            &[],
        );

        let eff = &decision.effective;
        if eff.max_input_tokens > 0
            && estimate_text_tokens(inp.content.len(), &eff.estimation_budgets)
                > i64::from(eff.max_input_tokens)
        {
            return Err(DomainError::out_of_range(
                Res::Message,
                "content",
                "INPUT_TOO_LONG",
                "The message exceeds the model input limit",
            ));
        }
        if !inp.images.is_empty() {
            if snapshot.kill_switches.disable_images {
                return Err(DomainError::feature_disabled("images"));
            }
            if !eff.supports_vision() {
                return Err(DomainError::invalid(
                    Res::Message,
                    "content_type",
                    "VISION_NOT_SUPPORTED",
                    "The model does not support image input",
                ));
            }
        }
        let floor_applied = i64::from(self.cfg.estimation_budgets.minimal_generation_floor)
            .min(decision.reserve.max_output_tokens_applied);
        Ok(Preflight {
            snapshot,
            limits,
            periods,
            decision,
            attachments: atts,
            boundary,
            floor_applied,
        })
    }

    fn knowledge_params(&self, file_search: bool, tenant: Uuid) -> Option<KnowledgeParams> {
        let k = &self.cfg.knowledge_search;
        if !k.enabled || file_search {
            return None;
        }
        let pid = k.provider_id.as_deref()?;
        let entry = self.providers.get(pid)?;
        if !matches!(
            entry.kind,
            ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
        ) {
            tracing::warn!(provider = %pid, "knowledge search provider kind is not supported");
            return None;
        }
        let api_version = entry.api_version.clone().filter(|v| !v.trim().is_empty())?;
        let resolved = self.providers.resolve(pid, tenant)?;
        Some(KnowledgeParams {
            alias: resolved.alias,
            api_version,
            vector_store_id: k.vector_store_id.clone()?,
        })
    }

    /// Context assembly, provider resolution and the provider request.
    pub async fn build_plan(
        &self,
        ctx: &SecurityContext,
        inp: &TurnInputs,
        pre: Preflight,
    ) -> Result<TurnPlan, PlanError> {
        let chat = &inp.chat;
        let scope = tenant_scope(chat.tenant_id);
        let conn = self.db.conn().map_err(PlanError::Other)?;
        let summary = summaries::find(&conn, &scope, chat.id)
            .await
            .map_err(PlanError::Other)?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let recent = match pre.boundary {
            Some(b) => messages::recent_for_context(
                &conn,
                &scope,
                chat.id,
                b,
                frontier,
                u64::from(self.cfg.context.recent_messages_limit),
            )
            .await
            .map_err(PlanError::Other)?,
            None => vec![],
        };
        let eff = &pre.decision.effective;
        let tools = pre.decision.reserve.tools;
        let knowledge = self.knowledge_params(tools.file_search, chat.tenant_id);
        let instructions = system_instructions(
            eff,
            tools,
            knowledge.is_some(),
            &self.cfg.context.web_search_guard,
            &self.cfg.context.file_search_guard,
            &self.cfg.knowledge_search.guard,
        );
        let plan = context::assemble(ContextInputs {
            model: eff,
            max_output_tokens_applied: pre.decision.reserve.max_output_tokens_applied,
            tools,
            system_instructions: instructions,
            summary: summary.as_ref().map(|s| s.summary_text.clone()),
            recent: recent
                .into_iter()
                .filter(|m| m.role == "user" || m.role == "assistant")
                .map(|m| HistoryMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
            user_message: &inp.content,
            image_count: inp.images.len(),
        })
        .map_err(|_| PlanError::ContextBudget)?;

        let target = self
            .providers
            .resolve(&eff.provider_id, chat.tenant_id)
            .ok_or_else(|| {
                PlanError::Other(DomainError::internal(format!(
                    "provider '{}' of model '{}' is not configured",
                    eff.provider_id, eff.id
                )))
            })?;

        let mut tool_specs = Vec::new();
        if tools.file_search
            && let Some(vs) = &pre.attachments.vector_store_id
        {
            tool_specs.push(ToolSpec::FileSearch {
                vector_store_ids: vec![vs.clone()],
                max_num_results: eff.max_num_results,
            });
        }
        if tools.web_search {
            tool_specs.push(ToolSpec::WebSearch {
                context_size: eff.web_search_context_size,
            });
        }
        if tools.code_interpreter {
            tool_specs.push(ToolSpec::CodeInterpreter {
                file_ids: pre.attachments.code_interpreter_file_ids.clone(),
            });
        }
        if knowledge.is_some() {
            tool_specs.push(ToolSpec::Function {
                name: "search_knowledge".to_owned(),
                description: "Search the organization knowledge base and return relevant excerpts."
                    .to_owned(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" },
                        "top_k": { "type": "integer", "description": "Maximum number of results" }
                    },
                    "required": ["query"]
                }),
            });
        }
        let feature = {
            let names: Vec<&str> = tool_specs.iter().map(ToolSpec::feature_name).collect();
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join("+")
            }
        };

        let mut input = Vec::new();
        if let Some(s) = &plan.summary {
            input.push(InputItem::text(Role::User, s.clone()));
        }
        for m in &plan.recent {
            let role = if m.role == "assistant" {
                Role::Assistant
            } else {
                Role::User
            };
            input.push(InputItem::text(role, m.content.clone()));
        }
        let mut parts = vec![ContentPart::Text(inp.content.clone())];
        for img in &inp.images {
            if let Some(fid) = &img.provider_file_id {
                parts.push(ContentPart::Image {
                    file_id: fid.clone(),
                    secondary_file_id: img.secondary_file_id.clone(),
                });
            }
        }
        input.push(InputItem::Message {
            role: Role::User,
            content: parts,
        });

        let request = LlmRequest {
            model: eff.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input,
            max_output_tokens: u32::try_from(pre.decision.reserve.max_output_tokens_applied)
                .unwrap_or(u32::MAX),
            tools: tool_specs,
            max_tool_calls: eff.max_tool_calls,
            api_params: eff.general_config.api_params.clone(),
            user: provider_user(ctx.subject_tenant_id(), ctx.subject_id()),
            metadata: RequestMetadata {
                tenant_id: ctx.subject_tenant_id().to_string(),
                user_id: ctx.subject_id().to_string(),
                chat_id: chat.id.to_string(),
                request_type: "chat",
                feature,
            },
            stream: true,
        };
        Ok(TurnPlan {
            pre,
            context: plan,
            target,
            request,
            summary,
            knowledge,
        })
    }
}

/// Error of `build_plan`.
#[derive(Debug)]
pub enum PlanError {
    ContextBudget,
    Other(DomainError),
}

impl PlanError {
    #[must_use]
    pub fn into_domain(self) -> DomainError {
        match self {
            Self::ContextBudget => DomainError::out_of_range(
                Res::Message,
                "content",
                "CONTEXT_BUDGET_EXCEEDED",
                "The mandatory context does not fit the model input budget",
            ),
            Self::Other(e) => e,
        }
    }
}
