//! Streaming turns: send-message preflight, idempotent replay, the provider
//! task and the shared setup used by retry / edit.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Set};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::context::{self, AssemblyInputs, ContextPlan, OrderKey};
use super::finalize::{FinalOutcome, TurnFinal};
use super::quota::{Periods, PreflightInputs, QuotaDecision, max_output_applied};
use super::{Service, provider_user};
use crate::domain::authz::actions;
use crate::domain::billing;
use crate::domain::clock;
use crate::domain::error::{DisabledFeature, DomainError, DomainResult};
use crate::infra::llm::provider::ChatTarget;
use crate::infra::llm::sanitize::sanitize;
use crate::infra::llm::types::{
    Annotation, ContentPart, LlmEvent, LlmRequest, ProviderErrorKind, ProviderFailure,
    RequestMetadata, ToolSpec,
};
use crate::infra::storage::entity::{attachment, chat, chat_turn, chat_vector_store, message};

/// How often `last_progress_at` is refreshed at most.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// A citation sent in the `citations` event.
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub web: bool,
    pub title: String,
    pub snippet: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub span: Option<(usize, usize)>,
}

/// One per-tier, per-period quota warning entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarning {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<OffsetDateTime>,
}

/// Payload of `done`.
#[derive(Debug, Clone, PartialEq)]
pub struct DoneInfo {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub downgrade: bool,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// Events of the public SSE contract.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Started {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        thread_summary_applied: Option<i32>,
    },
    Delta {
        reasoning: bool,
        content: String,
    },
    Tool {
        done: bool,
        name: String,
        details: serde_json::Value,
    },
    Citations(Vec<Citation>),
    Done(DoneInfo),
    Error {
        code: String,
        message: String,
    },
}

impl StreamEvent {
    /// `true` for `done` / `error`.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// `true` for content events that stop the pre-content pings.
    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }
}

/// Result of a stream setup.
pub enum StreamStart {
    /// Idempotent replay of a completed turn (all events, buffered).
    Replay(Vec<StreamEvent>),
    /// A live generation: events arrive on the receiver; dropping the
    /// guard (client disconnect) cancels the turn.
    Live(LiveStream),
}

/// A live stream handle.
pub struct LiveStream {
    pub events: mpsc::Receiver<StreamEvent>,
    pub cancel: CancellationToken,
}

/// Send-message request.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Everything the provider task and the finalization need.
pub struct TurnRun {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub assistant_message_id: Uuid,
    pub user_message_key: OrderKey,
    pub decision: QuotaDecision,
    pub target: ChatTarget,
    pub request: LlmRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub plan_tokens: i64,
    pub effective_budget: i64,
    pub messages_truncated: bool,
    pub has_summary: bool,
    pub knowledge: Option<KnowledgeParams>,
    pub started: Instant,
}

/// Knowledge search parameters of a turn (`search_knowledge` function
/// tool).
#[derive(Debug, Clone)]
pub struct KnowledgeParams {
    pub target: crate::infra::llm::provider::StorageTarget,
    pub vector_store_id: String,
    pub top_k: usize,
    pub max_chunk_chars: usize,
    pub max_calls: u32,
}

/// Name of the knowledge search function tool.
pub const KNOWLEDGE_TOOL: &str = "search_knowledge";

/// Result of the shared preflight.
pub(crate) struct Preflight {
    pub decision: QuotaDecision,
    pub snapshot: PolicySnapshot,
    pub images: Vec<attachment::Model>,
}

/// Inputs of the shared preflight.
pub(crate) struct PreflightArgs<'a> {
    pub ctx: &'a SecurityContext,
    pub chat: &'a chat::Model,
    pub scope: &'a AccessScope,
    pub content: &'a str,
    pub images: Vec<attachment::Model>,
    pub web_search: bool,
    /// Exclude the messages of this request from `prior_context_tokens`.
    pub exclude_request_id: Option<Uuid>,
}

impl Service {
    /// Ready attachment flags of a chat: `(has documents, has code files)`.
    pub(crate) async fn ready_attachment_flags<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> DomainResult<(bool, bool)> {
        let ready = attachment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq("ready"))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(attachment::Column::ForFileSearch.eq(true))
                            .add(attachment::Column::ForCodeInterpreter.eq(true)),
                    ),
            )
            .all(runner)
            .await?;
        Ok((
            ready.iter().any(|a| a.for_file_search),
            ready.iter().any(|a| a.for_code_interpreter),
        ))
    }

    /// `prior_context_tokens`: tokens of the latest assistant message with
    /// usage.
    pub(crate) async fn prior_context_tokens<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        chat_id: Uuid,
        exclude_request_id: Option<Uuid>,
    ) -> DomainResult<i64> {
        let mut cond = Condition::all();
        if let Some(r) = exclude_request_id {
            cond = cond.add(message::Column::RequestId.ne(r));
        }
        let m = message::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                cond.add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::Role.eq("assistant"))
                    .add(message::Column::DeletedAt.is_null())
                    .add(
                        Condition::any()
                            .add(message::Column::InputTokens.gt(0))
                            .add(message::Column::OutputTokens.gt(0)),
                    ),
            )
            .order_by(message::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(message::Column::Id, sea_orm::Order::Desc)
            .limit(1)
            .one(runner)
            .await?;
        Ok(m.map_or(0, |m| m.input_tokens + m.output_tokens))
    }

    /// Shared preflight of send / retry / edit: kill switches, quota
    /// cascade, tool quotas, input limit, image guards.
    pub(crate) async fn preflight(&self, args: PreflightArgs<'_>) -> DomainResult<Preflight> {
        let PreflightArgs {
            ctx,
            chat,
            scope,
            content,
            images,
            web_search,
            exclude_request_id,
        } = args;
        let snapshot = self.snapshot(ctx.subject_id()).await?;
        if snapshot.find_model(&chat.model).is_none() {
            return Err(DomainError::invalid_model(format!(
                "chat model '{}' is no longer in the catalog",
                chat.model
            )));
        }
        let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
        if image_count > self.cfg.rag.max_images_per_message {
            return Err(DomainError::TooManyImages {
                max: self.cfg.rag.max_images_per_message,
            });
        }
        let conn = self.db.conn()?;
        let prior = Self::prior_context_tokens(&conn, scope, chat.id, exclude_request_id).await?;
        if web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled {
                feature: DisabledFeature::WebSearch,
            });
        }
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let periods = Periods::at(clock::now());
        let usage =
            Self::usage_state(&conn, ctx.subject_tenant_id(), ctx.subject_id(), periods).await?;
        let (docs, code) = Self::ready_attachment_flags(&conn, scope, chat.id).await?;
        let decision = super::quota::resolve_effective_model(
            &PreflightInputs {
                selected_model: &chat.model,
                message_bytes: content.len(),
                prior_context_tokens: prior,
                image_count,
                has_ready_documents: docs,
                has_ready_code_files: code,
                web_search_requested: web_search,
            },
            &snapshot,
            &limits,
            &usage,
            self.cfg.streaming.max_output_tokens,
            self.cfg.estimation_budgets.minimal_generation_floor,
            periods,
        )?;
        self.check_tool_quotas(&usage, decision.tools)?;
        {
            let m = &self.metrics;
            let l = crate::infra::metrics::labels(&[
                (
                    "decision",
                    if decision.is_downgrade() {
                        "downgrade"
                    } else {
                        "allow"
                    },
                ),
                ("model", decision.effective.id.as_str()),
                ("tier", decision.effective.tier.as_str()),
            ]);
            m.quota_preflight.add(1, &l);
            #[allow(clippy::cast_precision_loss)]
            m.quota_estimated_tokens
                .record(decision.reserve.reserve_tokens as f64, &[]);
            m.image_inputs_per_turn.record(f64::from(image_count), &[]);
        }
        let effective = &decision.effective;
        if effective.max_input_tokens > 0 {
            let est = billing::estimate_text_tokens(content.len(), &effective.estimation_budgets);
            if est > i64::from(effective.max_input_tokens) {
                return Err(DomainError::InputTooLong {
                    limit: effective.max_input_tokens,
                });
            }
        }
        if image_count > 0 {
            if snapshot.kill_switches.disable_images {
                return Err(DomainError::FeatureDisabled {
                    feature: DisabledFeature::Images,
                });
            }
            if !effective.supports_vision() {
                return Err(DomainError::VisionNotSupported);
            }
        }
        Ok(Preflight {
            decision,
            snapshot,
            images,
        })
    }

    /// Context assembly, tools and provider request of a turn.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn build_request(
        &self,
        ctx_tenant: Uuid,
        ctx_user: Uuid,
        chat: &chat::Model,
        scope: &AccessScope,
        decision: &QuotaDecision,
        content: &str,
        images: &[attachment::Model],
        exclude_request_id: Option<Uuid>,
    ) -> DomainResult<(
        ContextPlan,
        LlmRequest,
        ChatTarget,
        HashMap<String, (Uuid, String)>,
        Option<KnowledgeParams>,
    )> {
        let conn = self.db.conn()?;
        let boundary = context::latest_message(&conn, scope, chat.id, exclude_request_id)
            .await?
            .map(|m| (m.created_at, m.id));
        let anthropic_alias = self
            .providers
            .anthropic_alias(&decision.effective.provider_id, ctx_tenant);
        let image_parts: Vec<ContentPart> = images
            .iter()
            .filter_map(|a| {
                a.provider_file_id
                    .clone()
                    .map(|file_id| ContentPart::Image {
                        file_id,
                        secondary_file_id: if anthropic_alias.is_some() {
                            a.secondary_file_id.clone()
                        } else {
                            None
                        },
                    })
            })
            .collect();
        let max_out = max_output_applied(&decision.effective, self.cfg.streaming.max_output_tokens);
        let plan = context::assemble(
            &conn,
            scope,
            AssemblyInputs {
                cfg: &self.cfg,
                model: &decision.effective,
                tools: decision.tools,
                max_output_applied: max_out,
                chat_id: chat.id,
                boundary,
                user_content: content,
                images: image_parts,
            },
        )
        .await?;
        let target = self
            .providers
            .chat_target(&decision.effective.provider_id, ctx_tenant)
            .map_err(|detail| DomainError::ProviderResolution { detail })?;

        // Tools.
        let mut tools = Vec::new();
        let mut file_map = HashMap::new();
        let mut features = Vec::new();
        if decision.tools.file_search {
            let store = chat_vector_store::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(
                    Condition::all()
                        .add(chat_vector_store::Column::ChatId.eq(chat.id))
                        .add(chat_vector_store::Column::VectorStoreId.is_not_null()),
                )
                .one(&conn)
                .await?;
            if let Some(vs) = store.and_then(|s| s.vector_store_id) {
                tools.push(ToolSpec::FileSearch {
                    vector_store_ids: vec![vs],
                    max_num_results: decision.effective.max_num_results,
                });
                features.push("file_search");
                let ready = attachment::Entity::find()
                    .secure()
                    .scope_with(scope)
                    .filter(
                        Condition::all()
                            .add(attachment::Column::ChatId.eq(chat.id))
                            .add(attachment::Column::Status.eq("ready"))
                            .add(attachment::Column::DeletedAt.is_null())
                            .add(attachment::Column::ProviderFileId.is_not_null()),
                    )
                    .all(&conn)
                    .await?;
                for a in ready {
                    if let Some(fid) = a.provider_file_id {
                        file_map.insert(fid, (a.id, a.filename));
                    }
                }
            }
        }
        if decision.tools.web_search {
            tools.push(ToolSpec::WebSearch {
                search_context_size: decision
                    .effective
                    .web_search_context_size
                    .as_str()
                    .to_owned(),
            });
            features.push("web_search");
        }
        if decision.tools.code_interpreter {
            let files: Vec<String> = attachment::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(chat.id))
                        .add(attachment::Column::ForCodeInterpreter.eq(true))
                        .add(attachment::Column::Status.eq("ready"))
                        .add(attachment::Column::DeletedAt.is_null()),
                )
                .order_by(attachment::Column::CreatedAt, sea_orm::Order::Asc)
                .all(&conn)
                .await?
                .into_iter()
                .filter_map(|a| a.provider_file_id)
                .collect();
            if !files.is_empty() {
                tools.push(ToolSpec::CodeInterpreter { file_ids: files });
                features.push("code_interpreter");
            }
        }
        let knowledge = if decision.tools.file_search
            && tools
                .iter()
                .any(|t| matches!(t, ToolSpec::FileSearch { .. }))
        {
            None
        } else {
            self.knowledge_params(ctx_tenant)
        };
        let mut instructions = plan.instructions.clone();
        if knowledge.is_some() {
            tools.push(ToolSpec::Function {
                name: KNOWLEDGE_TOOL.to_owned(),
                description: "Search the organization knowledge base and return relevant excerpts."
                    .to_owned(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "top_k": {"type": "integer", "description": "Maximum number of excerpts"}
                    },
                    "required": ["query"]
                }),
            });
            features.push("search_knowledge");
            let guard = &self.cfg.knowledge_search.guard;
            if !guard.is_empty() {
                if instructions.is_empty() {
                    instructions.clone_from(guard);
                } else {
                    instructions = format!("{instructions}\n\n{guard}");
                }
            }
        }
        let feature = if features.is_empty() {
            "none".to_owned()
        } else {
            features.join("+")
        };
        let max_out_u32 = u32::try_from(max_out).unwrap_or(u32::MAX);
        let max_tool_calls = (!tools.is_empty()).then_some(decision.effective.max_tool_calls);
        let request = LlmRequest {
            model: decision.effective.provider_model_id.clone(),
            instructions,
            input: plan.input.clone(),
            function_items: Vec::new(),
            tools,
            max_output_tokens: max_out_u32,
            max_tool_calls,
            api_params: decision.effective.general_config.api_params.clone(),
            user: provider_user(ctx_tenant, ctx_user),
            metadata: RequestMetadata {
                tenant_id: ctx_tenant.to_string(),
                user_id: ctx_user.to_string(),
                chat_id: chat.id.to_string(),
                request_type: "chat".to_owned(),
                feature,
            },
            stream: true,
        };
        Ok((plan, request, target, file_map, knowledge))
    }

    /// Knowledge search parameters, when enabled and buildable.
    fn knowledge_params(&self, tenant_id: Uuid) -> Option<KnowledgeParams> {
        let k = &self.cfg.knowledge_search;
        if !k.enabled {
            return None;
        }
        let (Some(pid), Some(vs)) = (&k.provider_id, &k.vector_store_id) else {
            return None;
        };
        let entry = self.providers.get(pid)?;
        let kind_ok = matches!(
            entry.entry.kind,
            crate::config::ProviderKind::OpenaiResponses
                | crate::config::ProviderKind::AnthropicMessages
        );
        let api_version = entry
            .entry
            .api_version
            .clone()
            .filter(|v| !v.trim().is_empty());
        if !kind_ok || api_version.is_none() {
            tracing::warn!(provider = %pid, "mini-chat: knowledge search parameters cannot be built; search_knowledge is off");
            return None;
        }
        let alias = self
            .providers
            .chat_target(pid, tenant_id)
            .ok()
            .map(|t| t.alias)?;
        Some(KnowledgeParams {
            target: crate::infra::llm::provider::StorageTarget {
                provider_id: pid.clone(),
                alias,
                storage_kind: crate::config::StorageKind::Azure,
                api_version,
                backend_label: pid.clone(),
            },
            vector_store_id: vs.clone(),
            top_k: k.top_k,
            max_chunk_chars: k.max_chunk_chars,
            max_calls: k.max_calls_per_message,
        })
    }

    /// Run one knowledge retrieval and format the function output.
    async fn knowledge_output(&self, k: &KnowledgeParams, arguments: &str) -> (String, bool) {
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or_default()
            .to_owned();
        let top_k = args
            .get("top_k")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0)
            .unwrap_or(k.top_k)
            .min(k.top_k);
        if query.trim().is_empty() {
            return (
                serde_json::json!({"error": "query is required"}).to_string(),
                false,
            );
        }
        let started = Instant::now();
        let res = self
            .storage
            .search_vector_store(&k.target, &k.vector_store_id, &query, top_k)
            .await;
        #[allow(clippy::cast_precision_loss)]
        self.metrics
            .knowledge_search_latency_ms
            .record(started.elapsed().as_millis() as f64, &[]);
        self.metrics.knowledge_search.add(
            1,
            &crate::infra::metrics::labels(&[("result", if res.is_ok() { "ok" } else { "error" })]),
        );
        match res {
            Ok(chunks) => {
                #[allow(clippy::cast_precision_loss)]
                self.metrics
                    .knowledge_search_chunks
                    .record(chunks.len() as f64, &[]);
                let results: Vec<serde_json::Value> = chunks
                    .into_iter()
                    .take(top_k)
                    .map(|c| {
                        let text: String = c.text.chars().take(k.max_chunk_chars).collect();
                        serde_json::json!({"text": text, "filename": c.filename, "score": c.score})
                    })
                    .collect();
                (serde_json::json!({"results": results}).to_string(), true)
            }
            Err(e) => {
                tracing::warn!(error = %e, "mini-chat: knowledge retrieval failed");
                (
                    serde_json::json!({"error": "knowledge search is temporarily unavailable"})
                        .to_string(),
                    false,
                )
            }
        }
    }

    /// Referenced attachments of the request (found in the chat).
    async fn referenced_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> DomainResult<Vec<attachment::Model>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db.conn()?;
        let rows = attachment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Id.is_in(ids.to_vec()))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .all(&conn)
            .await?;
        // keep request order
        let mut by_id: HashMap<Uuid, attachment::Model> =
            rows.into_iter().map(|a| (a.id, a)).collect();
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    /// Replay events of a completed turn.
    async fn replay_events(
        &self,
        scope: &AccessScope,
        chat: &chat::Model,
        turn: &chat_turn::Model,
    ) -> DomainResult<Vec<StreamEvent>> {
        let conn = self.db.conn()?;
        let base = Condition::all()
            .add(message::Column::ChatId.eq(chat.id))
            .add(message::Column::Role.eq("assistant"));
        let filter = match turn.assistant_message_id {
            Some(id) => base.add(message::Column::Id.eq(id)),
            None => base.add(message::Column::RequestId.eq(turn.request_id)),
        };
        let msg = message::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(filter)
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::internal("completed turn has no assistant message"))?;
        let effective = msg
            .model
            .clone()
            .or_else(|| turn.effective_model.clone())
            .unwrap_or_else(|| chat.model.clone());
        let downgrade = effective != chat.model;
        Ok(vec![
            StreamEvent::Started {
                request_id: turn.request_id,
                message_id: msg.id,
                is_new_turn: false,
                thread_summary_applied: None,
            },
            StreamEvent::Delta {
                reasoning: false,
                content: msg.content.clone(),
            },
            StreamEvent::Done(DoneInfo {
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

    /// `POST /v1/chats/{id}/messages:stream`.
    ///
    /// # Errors
    /// Every pre-stream rejection (JSON error).
    #[allow(clippy::too_many_lines)]
    pub async fn send_message(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> DomainResult<StreamStart> {
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let mut seen = HashSet::new();
        if req.attachment_ids.iter().any(|id| !seen.insert(*id)) {
            return Err(DomainError::invalid_attachment("duplicate attachment ids"));
        }
        let max_ids = self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message;
        if req.attachment_ids.len() > usize::try_from(max_ids).unwrap_or(usize::MAX) {
            return Err(DomainError::invalid_attachment("too many attachment ids"));
        }
        let (chat, scope) = self
            .authorized_chat(ctx, actions::SEND_MESSAGE, chat_id)
            .await?;
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);

        // Idempotency, then the parallel-turn guard.
        let conn = self.db.conn()?;
        if let Some(turn) = chat_turn::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .one(&conn)
            .await?
        {
            if turn.state == "completed" && turn.deleted_at.is_none() {
                return Ok(StreamStart::Replay(
                    self.replay_events(&scope, &chat, &turn).await?,
                ));
            }
            return Err(DomainError::RequestIdConflict {
                detail: format!(
                    "request_id {request_id} used by turn {} in state {} (deleted: {})",
                    turn.id,
                    turn.state,
                    turn.deleted_at.is_some()
                ),
            });
        }
        if Self::running_turn(&conn, &scope, chat_id).await?.is_some() {
            return Err(DomainError::TurnAlreadyRunning);
        }

        let referenced = self
            .referenced_attachments(&scope, chat_id, &req.attachment_ids)
            .await?;
        let images: Vec<attachment::Model> = referenced
            .into_iter()
            .filter(|a| a.attachment_kind == "image")
            .collect();
        let pre = self
            .preflight(PreflightArgs {
                ctx,
                chat: &chat,
                scope: &scope,
                content: &req.content,
                images,
                web_search: req.web_search,
                exclude_request_id: None,
            })
            .await?;
        let ready_images: Vec<attachment::Model> = pre
            .images
            .iter()
            .filter(|a| a.status == "ready")
            .cloned()
            .collect();
        let (plan, request, target, file_map, knowledge) = self
            .build_request(
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                &chat,
                &scope,
                &pre.decision,
                &req.content,
                &ready_images,
                None,
            )
            .await?;
        let _ = &pre.snapshot;

        // Reserve transaction.
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let decision = pre.decision.clone();
        let content = req.content.clone();
        let attachment_ids = req.attachment_ids.clone();
        let web_search = req.web_search;
        let scope_tx = scope.clone();
        let user_created = self
            .tx(move |tx| {
                let decision = decision.clone();
                let content = content.clone();
                let attachment_ids = attachment_ids.clone();
                let scope = scope_tx.clone();
                Box::pin(async move {
                    Self::write_reserve(tx, tenant_id, user_id, &decision).await?;
                    let now = clock::now();
                    insert_message(
                        tx,
                        &scope,
                        NewMessage {
                            id: user_message_id,
                            tenant_id,
                            chat_id,
                            request_id,
                            role: "user",
                            content: &content,
                            model: None,
                            usage: None,
                            provider_response_id: None,
                            created_at: now,
                        },
                    )
                    .await?;
                    touch_chat(tx, &scope, chat_id, now).await?;
                    link_attachments(
                        tx,
                        &scope,
                        tenant_id,
                        user_id,
                        chat_id,
                        user_message_id,
                        &attachment_ids,
                    )
                    .await?;
                    let am = new_turn(
                        turn_id,
                        tenant_id,
                        chat_id,
                        request_id,
                        user_id,
                        web_search,
                        Some(&decision),
                        now,
                    );
                    match secure_insert::<chat_turn::Entity>(am, &scope, tx).await {
                        Ok(_) => {}
                        Err(e) if e.is_unique_violation() => {
                            return Err(DomainError::UniqueViolation {
                                detail: "chat_turns".to_owned(),
                            });
                        }
                        Err(e) => return Err(e.into()),
                    }
                    Ok(now)
                })
            })
            .await;
        let user_created = match user_created {
            Ok(ts) => {
                self.record_reserve();
                ts
            }
            Err(DomainError::UniqueViolation { .. }) => {
                return Err(self.turn_insert_conflict(&scope, chat_id, request_id).await);
            }
            Err(e) => return Err(e),
        };

        let run = TurnRun {
            tenant_id,
            user_id,
            chat_id,
            turn_id,
            request_id,
            assistant_message_id: Uuid::new_v4(),
            user_message_key: (user_created, user_message_id),
            decision: pre.decision,
            target,
            request,
            file_map,
            plan_tokens: plan.assembled_tokens,
            effective_budget: plan.effective_budget,
            messages_truncated: plan.messages_truncated,
            has_summary: plan.summary.is_some(),
            knowledge,
            started: Instant::now(),
        };
        Ok(StreamStart::Live(
            self.spawn_turn(run, plan.summary_applied),
        ))
    }

    /// Classify a lost turn insert: an existing turn with the request id is
    /// `request_id_conflict`, otherwise `turn_already_running`.
    pub(crate) async fn turn_insert_conflict(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainError {
        let Ok(conn) = self.db.conn() else {
            return DomainError::TurnAlreadyRunning;
        };
        match chat_turn::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .one(&conn)
            .await
        {
            Ok(Some(t)) => DomainError::RequestIdConflict {
                detail: format!("request_id {request_id} taken by turn {}", t.id),
            },
            _ => DomainError::TurnAlreadyRunning,
        }
    }

    /// The running (non-deleted) turn of a chat.
    pub(crate) async fn running_turn<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> DomainResult<Option<chat_turn::Model>> {
        Ok(chat_turn::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::State.eq("running"))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .one(runner)
            .await?)
    }

    /// Spawn the provider task of a committed turn.
    pub(crate) fn spawn_turn(
        self: &Arc<Self>,
        run: TurnRun,
        summary_applied: Option<i32>,
    ) -> LiveStream {
        let capacity = usize::from(self.cfg.streaming.sse_channel_capacity.max(1));
        let (tx, rx) = mpsc::channel(capacity.max(2));
        let cancel = CancellationToken::new();
        let started = tx.try_send(StreamEvent::Started {
            request_id: run.request_id,
            message_id: run.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: summary_applied,
        });
        debug_assert!(started.is_ok(), "fresh channel accepts the first event");
        let svc = Arc::clone(self);
        let token = cancel.clone();
        tokio::spawn(async move {
            svc.run_turn(run, tx, token).await;
        });
        LiveStream { events: rx, cancel }
    }

    /// Provider task: relay translated provider events and finalize.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run_turn(
        self: Arc<Self>,
        run: TurnRun,
        tx: mpsc::Sender<StreamEvent>,
        cancel: CancellationToken,
    ) {
        let mut text = String::new();
        let mut annotations: Vec<Annotation> = Vec::new();
        let mut counts = ToolCounts::default();
        let mut last_progress = Instant::now();
        let mut ttft: Option<u64> = None;
        let ws_limit = self.cfg.quota.web_search_max_calls_per_message;
        let ci_limit = self.cfg.quota.code_interpreter_max_calls_per_message;
        let m = Arc::clone(&self.metrics);
        let pm = crate::infra::metrics::labels(&[
            ("provider", run.target.provider_id.as_str()),
            ("model", run.decision.effective.id.as_str()),
        ]);
        m.stream_started.add(1, &pm);
        m.active_streams.add(1, &[]);

        let mut request = run.request.clone();
        let mut iterations: u32 = 0;
        let mut knowledge_calls: u32 = 0;
        let outcome: FinalOutcome = 'run: loop {
            iterations += 1;
            if let Some(k) = &run.knowledge
                && iterations > k.max_calls + 2
            {
                break 'run FinalOutcome::Failed {
                    code: "agentic_iterations_exceeded".to_owned(),
                    message: "The model exceeded the tool iteration limit".to_owned(),
                    detail: None,
                    usage: None,
                    response_id: None,
                };
            }
            let stream = tokio::select! {
                () = cancel.cancelled() => break 'run FinalOutcome::Cancelled,
                r = self.llm.stream(&run.target, &request) => r,
            };
            let mut stream = match stream {
                Ok(s) => s,
                Err(f) => break 'run failed_from(&f),
            };
            loop {
                let ev = tokio::select! {
                    () = cancel.cancelled() => break 'run FinalOutcome::Cancelled,
                    ev = stream.next() => ev,
                };
                let Some(ev) = ev else {
                    break 'run failed_from(&ProviderFailure::provider(
                        "provider stream ended without a terminal event",
                    ));
                };
                let mut out: Option<StreamEvent> = None;
                match ev {
                    LlmEvent::TextDelta(t) => {
                        if ttft.is_none() {
                            ttft = Some(
                                u64::try_from(run.started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX),
                            );
                            #[allow(clippy::cast_precision_loss)]
                            m.ttft_provider_ms
                                .record(ttft.unwrap_or_default() as f64, &pm);
                        }
                        text.push_str(&t);
                        out = Some(StreamEvent::Delta {
                            reasoning: false,
                            content: t,
                        });
                    }
                    LlmEvent::ReasoningDelta(t) => {
                        out = Some(StreamEvent::Delta {
                            reasoning: true,
                            content: t,
                        });
                    }
                    LlmEvent::ToolStart { name, details } => {
                        match name.as_str() {
                            "web_search" => counts.web_search_started += 1,
                            "code_interpreter" => counts.code_interpreter_started += 1,
                            _ => {}
                        }
                        if counts.web_search_started > ws_limit {
                            break 'run FinalOutcome::Failed {
                                code: "web_search_calls_exceeded".to_owned(),
                                message:
                                    "The model exceeded the web search call limit for one message"
                                        .to_owned(),
                                detail: None,
                                usage: None,
                                response_id: None,
                            };
                        }
                        if counts.code_interpreter_started > ci_limit {
                            break 'run FinalOutcome::Failed {
                                code: "code_interpreter_calls_exceeded".to_owned(),
                                message: "The model exceeded the code interpreter call limit for one message".to_owned(),
                                detail: None,
                                usage: None,
                                response_id: None,
                            };
                        }
                        out = Some(StreamEvent::Tool {
                            done: false,
                            name,
                            details,
                        });
                    }
                    LlmEvent::ToolDone { name, details } => {
                        let column = match name.as_str() {
                            "web_search" => {
                                counts.web_search_done += 1;
                                Some(chat_turn::Column::WebSearchCompletedCount)
                            }
                            "code_interpreter" => {
                                counts.code_interpreter_done += 1;
                                Some(chat_turn::Column::CodeInterpreterCompletedCount)
                            }
                            "file_search" => {
                                counts.file_search_done += 1;
                                Some(chat_turn::Column::FileSearchCompletedCount)
                            }
                            _ => None,
                        };
                        if let Some(col) = column {
                            self.bump_turn_counter(&run, col).await;
                        }
                        out = Some(StreamEvent::Tool {
                            done: true,
                            name,
                            details,
                        });
                    }
                    LlmEvent::Annotation(a) => annotations.push(a),
                    LlmEvent::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    } if name == KNOWLEDGE_TOOL && run.knowledge.is_some() => {
                        let Some(k) = &run.knowledge else {
                            continue;
                        };
                        knowledge_calls += 1;
                        let output = if knowledge_calls > k.max_calls {
                            serde_json::json!({"error": "search limit reached; answer with the information you have"})
                                .to_string()
                        } else {
                            let (out, ok) = self.knowledge_output(k, &arguments).await;
                            if ok {
                                self.bump_turn_counter(
                                    &run,
                                    chat_turn::Column::FileSearchCompletedCount,
                                )
                                .await;
                            }
                            out
                        };
                        request
                            .function_items
                            .push(crate::infra::llm::types::FunctionItem::Call {
                                call_id: call_id.clone(),
                                name,
                                arguments,
                            });
                        request.function_items.push(
                            crate::infra::llm::types::FunctionItem::Output { call_id, output },
                        );
                        counts.knowledge_calls = knowledge_calls;
                        continue 'run;
                    }
                    LlmEvent::FunctionCall { name, .. } => {
                        break 'run FinalOutcome::Failed {
                            code: "unexpected_tool_use".to_owned(),
                            message: "The model requested a tool that is not available".to_owned(),
                            detail: Some(format!("function call '{name}'")),
                            usage: None,
                            response_id: None,
                        };
                    }
                    LlmEvent::Completed(c) => {
                        if let Some(reason) = &c.incomplete_reason {
                            tracing::warn!(%reason, turn_id = %run.turn_id, "mini-chat: stream incomplete");
                        }
                        break 'run FinalOutcome::Completed {
                            usage: c.usage,
                            response_id: c.response_id,
                            incomplete: c.incomplete_reason.is_some(),
                        };
                    }
                    LlmEvent::Failed(f) => break 'run failed_from(&f),
                }
                if let Some(ev) = out {
                    let is_content = ev.is_content();
                    let sent = tokio::select! {
                        () = cancel.cancelled() => break 'run FinalOutcome::Cancelled,
                        r = tx.send(ev) => r,
                    };
                    if sent.is_err() {
                        break 'run FinalOutcome::Cancelled;
                    }
                    if is_content && last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        self.refresh_progress(&run).await;
                    }
                }
            }
        };

        match &outcome {
            FinalOutcome::Completed { incomplete, .. } => {
                m.stream_completed.add(1, &pm);
                if *incomplete {
                    m.stream_incomplete.add(1, &pm);
                }
            }
            FinalOutcome::Failed { code, .. } => {
                let mut l = pm.clone();
                l.push(opentelemetry::KeyValue::new("error_code", code.clone()));
                m.stream_failed.add(1, &l);
            }
            FinalOutcome::Cancelled => {
                let trig = crate::infra::metrics::labels(&[("trigger", "disconnect")]);
                m.cancel_requested.add(1, &trig);
                m.cancel_effective.add(1, &trig);
                m.streams_aborted.add(
                    1,
                    &crate::infra::metrics::labels(&[("trigger", "client_disconnect")]),
                );
                let stage = if text.is_empty() {
                    "before_first_token"
                } else {
                    "mid_stream"
                };
                m.stream_disconnected
                    .add(1, &crate::infra::metrics::labels(&[("stage", stage)]));
                m.time_to_abort_ms.record(0.0, &trig);
            }
        }
        let completed_ok = matches!(
            outcome,
            FinalOutcome::Completed {
                incomplete: false,
                ..
            }
        );
        let fin = TurnFinal {
            text,
            counts,
            ttft_ms: ttft,
        };
        // Citations (completed streams only), before the terminal event.
        if completed_ok && !tx.is_closed() {
            let items = Self::map_citations(&run, &annotations);
            if !items.is_empty() && tx.send(StreamEvent::Citations(items)).await.is_err() {
                tracing::debug!("mini-chat: client gone before citations");
            }
        }
        let fin_start = Instant::now();
        let terminal = self.finalize_turn(&run, outcome, fin).await;
        #[allow(clippy::cast_precision_loss)]
        {
            m.finalization_latency_ms
                .record(fin_start.elapsed().as_millis() as f64, &[]);
            m.stream_total_latency_ms
                .record(run.started.elapsed().as_millis() as f64, &pm);
        }
        m.active_streams.add(-1, &[]);
        if let Some(ev) = terminal
            && !tx.is_closed()
            && tx.send(ev).await.is_err()
        {
            tracing::debug!("mini-chat: client gone before the terminal event");
        }
    }

    async fn refresh_progress(&self, run: &TurnRun) {
        let Ok(conn) = self.db.conn() else { return };
        let res = chat_turn::Entity::update_many()
            .col_expr(
                chat_turn::Column::LastProgressAt,
                Expr::value(Some(clock::now())),
            )
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(run.turn_id))
                    .add(chat_turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(run.tenant_id))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "mini-chat: progress refresh failed");
        }
    }

    async fn bump_turn_counter(&self, run: &TurnRun, col: chat_turn::Column) {
        let Ok(conn) = self.db.conn() else { return };
        let now = clock::now();
        let res = chat_turn::Entity::update_many()
            .col_expr(col, sea_orm::ExprTrait::add(Expr::col(col), 1))
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(now)))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(run.turn_id))
                    .add(chat_turn::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(run.tenant_id))
            .exec(&conn)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "mini-chat: tool counter update failed");
        }
    }

    fn map_citations(run: &TurnRun, annotations: &[Annotation]) -> Vec<Citation> {
        let mut out = Vec::new();
        let mut seen_files = HashSet::new();
        for a in annotations {
            match a {
                Annotation::File { file_id, .. } => {
                    if let Some((attachment_id, filename)) = run.file_map.get(file_id)
                        && seen_files.insert(*attachment_id)
                    {
                        out.push(Citation {
                            web: false,
                            title: filename.clone(),
                            snippet: String::new(),
                            url: None,
                            attachment_id: Some(*attachment_id),
                            span: None,
                        });
                    }
                }
                Annotation::Url {
                    url,
                    title,
                    start,
                    end,
                    text,
                    part_text,
                } => {
                    let span = match (start, end) {
                        (Some(s), Some(e)) => Some((*s, *e)),
                        _ => None,
                    };
                    let snippet = text.clone().unwrap_or_else(|| match (span, part_text) {
                        (Some((s, e)), Some(p)) if s <= e => {
                            let chars: Vec<char> = p.chars().collect();
                            if e <= chars.len() {
                                chars[s..e].iter().collect()
                            } else {
                                String::new()
                            }
                        }
                        _ => String::new(),
                    });
                    out.push(Citation {
                        web: true,
                        title: title.clone(),
                        snippet,
                        url: Some(url.clone()),
                        attachment_id: None,
                        span,
                    });
                }
            }
        }
        out
    }
}

/// Tool counters of a turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCounts {
    /// `search_knowledge` calls of the turn (counted before retrieval).
    pub knowledge_calls: u32,
    pub web_search_started: u32,
    pub code_interpreter_started: u32,
    pub web_search_done: u32,
    pub code_interpreter_done: u32,
    pub file_search_done: u32,
}

fn failed_from(f: &ProviderFailure) -> FinalOutcome {
    let code = f.kind.code().to_owned();
    let message = match &f.kind {
        ProviderErrorKind::RateLimited {
            retry_after_secs: Some(s),
        } => format!(
            "The provider rate limit was exceeded; retry after {s} seconds: {}",
            sanitize(&f.message)
        ),
        ProviderErrorKind::RateLimited { .. } => {
            format!(
                "The provider rate limit was exceeded: {}",
                sanitize(&f.message)
            )
        }
        ProviderErrorKind::Timeout => "The provider request timed out".to_owned(),
        ProviderErrorKind::Provider => sanitize(&f.message),
    };
    FinalOutcome::Failed {
        code,
        message,
        detail: Some(format!(
            "{}{}",
            f.provider_code
                .as_deref()
                .map(|c| format!("[{c}] "))
                .unwrap_or_default(),
            f.message
        )),
        usage: f.usage,
        response_id: f.response_id.clone(),
    }
}

/// New message row values.
pub(crate) struct NewMessage<'a> {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub role: &'a str,
    pub content: &'a str,
    pub model: Option<String>,
    pub usage: Option<crate::infra::llm::types::Usage>,
    pub provider_response_id: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Insert a message row.
pub(crate) async fn insert_message<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    m: NewMessage<'_>,
) -> DomainResult<message::Model> {
    let u = m.usage.unwrap_or_default();
    let am = message::ActiveModel {
        id: Set(m.id),
        tenant_id: Set(m.tenant_id),
        chat_id: Set(m.chat_id),
        request_id: Set(Some(m.request_id)),
        role: Set(m.role.to_owned()),
        content: Set(m.content.to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(m.provider_response_id),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(u.input_tokens.max(0)),
        output_tokens: Set(u.output_tokens.max(0)),
        cache_read_input_tokens: Set(u.cache_read_input_tokens.max(0)),
        cache_write_input_tokens: Set(u.cache_write_input_tokens.max(0)),
        reasoning_tokens: Set(u.reasoning_tokens.max(0)),
        model: Set(m.model),
        is_compressed: Set(false),
        created_at: Set(m.created_at),
        deleted_at: Set(None),
    };
    Ok(secure_insert::<message::Entity>(am, scope, runner).await?)
}

/// Bump `chats.updated_at`.
pub(crate) async fn touch_chat<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    chat_id: Uuid,
    now: OffsetDateTime,
) -> DomainResult<()> {
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::value(now))
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(())
}

/// Validate `attachment_ids` and link them to the user message.
///
/// # Errors
/// `InvalidAttachment` for a missing, foreign, deleted or not-ready id.
pub(crate) async fn link_attachments<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
) -> DomainResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let rows = attachment::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(attachment::Column::Id.is_in(ids.to_vec()))
                .add(attachment::Column::DeletedAt.is_null()),
        )
        .all(runner)
        .await?;
    let by_id: HashMap<Uuid, attachment::Model> = rows.into_iter().map(|a| (a.id, a)).collect();
    for id in ids {
        let Some(a) = by_id.get(id) else {
            return Err(DomainError::invalid_attachment(format!(
                "attachment {id} not found"
            )));
        };
        if a.tenant_id != tenant_id
            || a.chat_id != chat_id
            || a.uploaded_by_user_id != user_id
            || a.status != "ready"
        {
            return Err(DomainError::invalid_attachment(format!(
                "attachment {id} is not a ready attachment of this chat"
            )));
        }
    }
    insert_links(runner, scope, tenant_id, chat_id, message_id, ids).await
}

/// Insert `message_attachments` rows in order.
pub(crate) async fn insert_links<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
) -> DomainResult<()> {
    use crate::infra::storage::entity::message_attachment;
    for id in ids {
        let am = message_attachment::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(*id),
            created_at: Set(clock::now()),
        };
        secure_insert::<message_attachment::Entity>(am, scope, runner).await?;
    }
    Ok(())
}

/// A new `running` turn row (reserve fields from the decision, or NULL for
/// a retry / edit turn).
#[allow(clippy::too_many_arguments)]
pub(crate) fn new_turn(
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    user_id: Uuid,
    web_search: bool,
    decision: Option<&QuotaDecision>,
    now: OffsetDateTime,
) -> chat_turn::ActiveModel {
    chat_turn::ActiveModel {
        id: Set(id),
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
        reserve_tokens: Set(decision.map(|d| d.reserve.reserve_tokens)),
        max_output_tokens_applied: Set(decision
            .map(|d| i32::try_from(d.reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
        reserved_credits_micro: Set(decision.map(|d| d.reserve.reserved_credits_micro)),
        policy_version_applied: Set(
            decision.map(|d| i64::try_from(d.policy_version).unwrap_or(i64::MAX))
        ),
        effective_model: Set(decision.map(|d| d.effective.id.clone())),
        minimal_generation_floor_applied: Set(
            decision.map(|d| i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX))
        ),
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
    }
}

/// Entry lookup helper used by tests.
#[must_use]
pub fn model_supports_tools(model: &ModelCatalogEntry) -> bool {
    let t = model.tool_support();
    t.web_search || t.file_search || t.code_interpreter
}
