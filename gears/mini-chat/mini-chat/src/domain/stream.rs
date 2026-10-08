//! Send-message streaming pipeline (DESIGN §3.6 "Send Message with Streaming
//! Response"), shared by `messages:stream`, retry and edit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::context::{self, HistoryMsg, input_limit};
use crate::domain::credits::estimate_text_tokens;
use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::domain::events::{CitationView, DoneView, StreamEvent};
use crate::domain::finalize::{
    FinalizeInput, FinalizeOutcome, MESSAGE_PERSISTENCE_MARKER, ThreadSummaryPayload, TurnBilling, book_reserve,
};
use crate::domain::quota::{CascadeDecision, Periods, RequestFacts, quota_status, resolve_effective_model};
use crate::domain::state::{AppState, ChatScopes};
use crate::infra::db::entities::{attachments, chat_turns, chats, messages};
use crate::infra::db::repo::{self, NewMessage, NewTurn, Preflight as PreflightCols};
use crate::infra::llm::resolver::ChatTarget;
use crate::infra::llm::{ChatItem, FileSearchTool, ItemRole, LlmEvent, LlmRequest, RawCitation, RequestMetadata, ToolsSpec, Usage, user_field};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Body of `messages:stream`.
#[derive(Debug, Clone)]
pub struct SendInput {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// A live stream: events plus the cancellation token tied to the client
/// connection.
pub struct LiveStream {
    pub rx: mpsc::Receiver<StreamEvent>,
    pub cancel: CancellationToken,
}

pub enum StreamStart {
    Replay(Vec<StreamEvent>),
    Live(LiveStream),
}

/// Result of the quota/policy preflight.
pub struct Preflight {
    pub snap: PolicySnapshot,
    pub limits: UserLimits,
    pub periods: Periods,
    pub decision: CascadeDecision,
    pub chat_atts: Vec<attachments::Model>,
    pub images: Vec<attachments::Model>,
    pub floor: i64,
}

/// Knowledge search parameters of a turn.
#[derive(Debug, Clone)]
pub struct KnowledgeTarget {
    pub alias: String,
    pub api_version: String,
    pub vector_store_id: String,
}

/// Assembled provider request and the data finalization needs.
pub struct Assembled {
    pub knowledge: Option<KnowledgeTarget>,
    pub target: ChatTarget,
    pub request: LlmRequest,
    pub file_map: HashMap<String, (Uuid, String)>,
    pub summary_tokens: Option<i64>,
    pub summary_trigger: SummaryTrigger,
}

/// Inputs of the thread-summary trigger evaluated at finalization.
#[derive(Debug, Clone, Default)]
pub struct SummaryTrigger {
    pub messages_truncated: bool,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub summary_exists: bool,
    pub frontier: Option<(time::OffsetDateTime, Uuid)>,
}

/// One running turn handed to the provider task.
pub struct TurnRun {
    pub ctx: SecurityContext,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub assistant_message_id: Uuid,
    pub selected_model: String,
    pub effective: ModelCatalogEntry,
    pub decision: CascadeDecision,
    pub billing: TurnBilling,
    pub assembled: Assembled,
}

fn turn_already_running() -> DomainError {
    DomainError::aborted(Res::Chat, "turn_already_running", "A generation is already running for this chat")
}

fn request_id_conflict() -> DomainError {
    DomainError::aborted(Res::Chat, "request_id_conflict", "The request_id is already used by another turn")
}

/// Validate content (non-empty after trim).
///
/// # Errors
/// 400 `EMPTY_CONTENT`.
pub fn validate_content(content: &str) -> DomainResult<()> {
    if content.trim().is_empty() {
        return Err(DomainError::empty_content());
    }
    Ok(())
}

impl AppState {
    /// Replay events of a completed turn (side-effect free).
    pub(crate) async fn replay_events(
        &self,
        tenant_scope: &AccessScope,
        chat: &chats::Model,
        turn: &chat_turns::Model,
    ) -> DomainResult<Vec<StreamEvent>> {
        let conn = self.db.conn()?;
        let msgs = repo::messages_of_request(&conn, tenant_scope, chat.id, turn.request_id).await?;
        let assistant = turn
            .assistant_message_id
            .and_then(|id| msgs.iter().find(|m| m.id == id).cloned())
            .or_else(|| msgs.iter().find(|m| m.role == "assistant").cloned());
        let Some(a) = assistant else {
            return Err(DomainError::internal("completed turn without assistant message"));
        };
        let effective = a.model.clone().or_else(|| turn.effective_model.clone()).unwrap_or_default();
        let downgraded = effective != chat.model;
        Ok(vec![
            StreamEvent::StreamStarted {
                request_id: turn.request_id,
                message_id: a.id,
                is_new_turn: false,
                thread_summary_tokens: None,
            },
            StreamEvent::Delta {
                kind: "text",
                content: a.content.clone(),
            },
            StreamEvent::Done(DoneView {
                input_tokens: a.input_tokens,
                output_tokens: a.output_tokens,
                effective_model: effective,
                selected_model: chat.model.clone(),
                downgraded,
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ])
    }

    /// Quota/policy preflight for a turn (no writes).
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn preflight(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        tenant_scope: &AccessScope,
        text: &str,
        attachment_ids: &[Uuid],
        web_search: bool,
    ) -> DomainResult<Preflight> {
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        if snap.find_model(&chat.model).is_none() {
            return Err(DomainError::invalid_model());
        }
        if web_search && snap.kill_switches.disable_web_search {
            return Err(DomainError::feature_disabled("web_search"));
        }
        let conn = self.db.conn()?;
        let chat_atts = repo::chat_attachments(&conn, tenant_scope, chat.id).await?;
        let ready_docs = chat_atts
            .iter()
            .any(|a| a.status == "ready" && a.for_file_search && a.attachment_kind == "document");
        let ready_code = chat_atts.iter().any(|a| a.status == "ready" && a.for_code_interpreter);
        let images: Vec<attachments::Model> = chat_atts
            .iter()
            .filter(|a| attachment_ids.contains(&a.id) && a.attachment_kind == "image")
            .cloned()
            .collect();
        let prior = repo::prior_context_tokens(&conn, tenant_scope, chat.id).await?;
        let limits = self.policy.user_limits(ctx.subject_id(), snap.policy_version).await?;
        let periods = Periods::at(repo::now());
        let usage = repo::read_usage(&conn, ctx.subject_tenant_id(), ctx.subject_id(), periods).await?;
        let facts = RequestFacts {
            message: text.to_owned(),
            prior_context_tokens: prior,
            image_count: images.len(),
            web_search_requested: web_search,
            chat_has_ready_documents: ready_docs,
            chat_has_ready_code_files: ready_code,
            cfg_max_output_tokens: self.cfg.streaming.max_output_tokens,
        };
        let decision = resolve_effective_model(&chat.model, &snap, &limits, &usage, &facts)
            .ok_or_else(|| DomainError::quota_exceeded("tokens"))?;
        let q = &self.cfg.quota;
        if decision.tools.web_search && usage.total_daily.web_search_calls >= i64::from(q.web_search_daily_quota) {
            return Err(DomainError::quota_exceeded("web_search"));
        }
        if decision.tools.code_interpreter
            && usage.total_daily.code_interpreter_calls >= i64::from(q.code_interpreter_daily_quota)
        {
            return Err(DomainError::quota_exceeded("code_interpreter"));
        }
        let eff = &decision.effective;
        if eff.max_input_tokens > 0 && estimate_text_tokens(text, &eff.estimation_budgets) > i64::from(eff.max_input_tokens) {
            return Err(DomainError::out_of_range(
                Res::Chat,
                "content",
                "INPUT_TOO_LONG",
                "the message exceeds the model's input token limit",
            ));
        }
        if !images.is_empty() {
            if images.len() > self.cfg.rag.max_images_per_message as usize {
                return Err(DomainError::out_of_range(
                    Res::Chat,
                    "image_count",
                    "TOO_MANY_IMAGES",
                    "too many images in one message",
                ));
            }
            if snap.kill_switches.disable_images {
                return Err(DomainError::feature_disabled("images"));
            }
            if !eff.supports_vision() {
                return Err(DomainError::field(
                    Res::Chat,
                    "content_type",
                    "VISION_NOT_SUPPORTED",
                    "the model does not support image input",
                ));
            }
        }
        let floor = i64::from(self.cfg.estimation_budgets.minimal_generation_floor)
            .min(decision.reserve.max_output_tokens_applied);
        Ok(Preflight {
            snap,
            limits,
            periods,
            decision,
            chat_atts,
            images,
            floor,
        })
    }

    /// Context assembly and provider resolution.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn assemble_request(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        tenant_scope: &AccessScope,
        pre: &Preflight,
        user_text: &str,
        exclude_message: Option<Uuid>,
    ) -> DomainResult<Assembled> {
        let eff = &pre.decision.effective;
        let conn = self.db.conn()?;
        let summary = repo::find_summary(&conn, tenant_scope, chat.id).await?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let all = repo::list_live_messages(&conn, tenant_scope, chat.id).await?;
        let mut history: Vec<HistoryMsg> = all
            .iter()
            .filter(|m| m.request_id.is_some() && !m.is_compressed && Some(m.id) != exclude_message)
            .filter(|m| frontier.is_none_or(|(fc, fid)| (m.created_at, m.id) > (fc, fid)))
            .map(|m| HistoryMsg {
                id: m.id,
                created_at: m.created_at,
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();
        let k = self.cfg.context.recent_messages_limit as usize;
        if history.len() > k {
            history.drain(..history.len() - k);
        }

        // Tools actually sent.
        let mut tools = ToolsSpec::default();
        let mut file_map = HashMap::new();
        if pre.decision.tools.file_search {
            let vs = repo::find_vector_store(&conn, tenant_scope, chat.tenant_id, chat.id).await?;
            if let Some(vs_id) = vs.and_then(|v| v.vector_store_id) {
                tools.file_search = Some(FileSearchTool {
                    vector_store_ids: vec![vs_id],
                    max_num_results: eff.max_num_results.max(1),
                });
                for a in &pre.chat_atts {
                    if a.status == "ready"
                        && let Some(f) = &a.provider_file_id
                    {
                        file_map.insert(f.clone(), (a.id, a.filename.clone()));
                    }
                }
            }
        }
        if pre.decision.tools.web_search {
            tools.web_search = Some(eff.web_search_context_size.as_str().to_owned());
        }
        if pre.decision.tools.code_interpreter {
            let files: Vec<String> = pre
                .chat_atts
                .iter()
                .filter(|a| a.status == "ready" && a.for_code_interpreter)
                .filter_map(|a| a.provider_file_id.clone())
                .collect();
            if !files.is_empty() {
                tools.code_interpreter = Some(files);
            }
        }

        let knowledge = if tools.file_search.is_none() && !pre.decision.tools.file_search {
            self.knowledge_target(ctx.subject_tenant_id())
        } else {
            None
        };
        tools.knowledge = knowledge.is_some();
        let mut instructions = eff.system_prompt.clone();
        let mut push_guard = |g: &str| {
            if !g.is_empty() {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(g);
            }
        };
        if tools.file_search.is_some() {
            push_guard(&self.cfg.context.file_search_guard);
        }
        if tools.web_search.is_some() {
            push_guard(&self.cfg.context.web_search_guard);
        }
        if tools.knowledge {
            push_guard(&self.cfg.knowledge_search.guard);
        }

        let max_out = pre.decision.reserve.max_output_tokens_applied;
        let limit = input_limit(eff, max_out).ok_or_else(|| {
            DomainError::out_of_range(
                Res::Chat,
                "content",
                "CONTEXT_BUDGET_EXCEEDED",
                "max_output_tokens leaves no input budget",
            )
        })?;
        let b = &eff.estimation_budgets;
        let mut surcharges = 0_i64;
        if pre.decision.tools.file_search {
            surcharges += i64::from(b.tool_surcharge_tokens);
        }
        if pre.decision.tools.web_search {
            surcharges += i64::from(b.web_search_surcharge_tokens);
        }
        if pre.decision.tools.code_interpreter {
            surcharges += i64::from(b.code_interpreter_surcharge_tokens);
        }
        let budget = limit
            .saturating_sub(surcharges)
            .saturating_sub(i64::from(b.fixed_overhead_tokens));
        let plan = context::assemble(
            &instructions,
            summary.as_ref().map(|s| s.summary_text.as_str()),
            &history,
            user_text,
            pre.images.len(),
            b,
            budget,
        )?;

        let target = self
            .resolver
            .chat_target(&eff.provider_id, ctx.subject_tenant_id(), &eff.provider_model_id)?;
        let mut items = plan.items.clone();
        let image_files: Vec<String> = pre.images.iter().filter_map(|a| a.provider_file_id.clone()).collect();
        items.push(ChatItem {
            role: ItemRole::User,
            text: user_text.to_owned(),
            images: image_files,
        });
        let feature = tools.feature();
        let request = LlmRequest {
            model: if eff.provider_model_id.is_empty() { eff.id.clone() } else { eff.provider_model_id.clone() },
            instructions,
            items,
            max_output_tokens: u32::try_from(max_out).unwrap_or(u32::MAX),
            tools,
            max_tool_calls: eff.max_tool_calls,
            api_params: eff.general_config.api_params.clone(),
            user: user_field(ctx.subject_tenant_id(), ctx.subject_id()),
            metadata: RequestMetadata {
                tenant_id: ctx.subject_tenant_id().to_string(),
                user_id: ctx.subject_id().to_string(),
                chat_id: chat.id.to_string(),
                request_type: "chat",
                feature,
            },
            stream: true,
            extra_input: Vec::new(),
        };
        Ok(Assembled {
            knowledge,
            target,
            request,
            file_map,
            summary_tokens: if plan.summary_included {
                summary.as_ref().map(|s| i64::from(s.token_estimate))
            } else {
                None
            },
            summary_trigger: SummaryTrigger {
                messages_truncated: plan.messages_truncated,
                assembled_tokens: plan.assembled_tokens,
                effective_budget: limit,
                summary_exists: summary.is_some(),
                frontier,
            },
        })
    }

    /// Knowledge search parameters, when the feature is enabled and usable.
    fn knowledge_target(&self, tenant: Uuid) -> Option<KnowledgeTarget> {
        let ks = &self.cfg.knowledge_search;
        if !ks.enabled {
            return None;
        }
        let provider_id = ks.provider_id.as_deref()?;
        let entry = self.resolver.providers().get(provider_id)?;
        if !matches!(entry.kind, crate::config::ProviderKind::OpenaiResponses | crate::config::ProviderKind::AnthropicMessages) {
            tracing::warn!(provider = %provider_id, "knowledge search provider kind not supported; knowledge search off");
            return None;
        }
        let Some(api_version) = entry.api_version.clone().filter(|v| !v.trim().is_empty()) else {
            tracing::warn!(provider = %provider_id, "knowledge search provider has no api_version; knowledge search off");
            return None;
        };
        let alias = self.resolver.chat_target(provider_id, tenant, "").ok()?.alias;
        Some(KnowledgeTarget {
            alias,
            api_version,
            vector_store_id: ks.vector_store_id.clone()?,
        })
    }

    /// Run one `search_knowledge` call; returns the function output text and
    /// whether the retrieval succeeded.
    async fn run_knowledge_call(&self, k: &KnowledgeTarget, arguments: &str, calls_so_far: u32) -> (String, bool) {
        let ks = &self.cfg.knowledge_search;
        if calls_so_far > ks.max_calls_per_message {
            return (
                serde_json::json!({"error": "search limit reached; answer with the information you already have"}).to_string(),
                false,
            );
        }
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let query = args.get("query").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let top_k = args
            .get("top_k")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(ks.top_k)
            .clamp(1, ks.top_k);
        match self
            .storage
            .search_vector_store(&k.alias, &k.api_version, &k.vector_store_id, &query, top_k, ks.max_chunk_chars)
            .await
        {
            Ok(chunks) => (serde_json::json!({"results": chunks}).to_string(), true),
            Err(e) => {
                tracing::warn!(error = %e, "knowledge search failed");
                (serde_json::json!({"error": "knowledge search failed"}).to_string(), false)
            }
        }
    }

    /// `POST /v1/chats/{id}/messages:stream`.
    #[allow(clippy::too_many_lines)]
    pub async fn start_send(self: &Arc<Self>, ctx: SecurityContext, chat_id: Uuid, input: SendInput) -> DomainResult<StreamStart> {
        validate_content(&input.content)?;
        let mut seen = std::collections::HashSet::new();
        if !input.attachment_ids.iter().all(|a| seen.insert(*a)) {
            return Err(DomainError::invalid_attachment("duplicate attachment id"));
        }
        let max_ids = (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if input.attachment_ids.len() > max_ids {
            return Err(DomainError::invalid_attachment("too many attachment ids"));
        }
        let scopes = self.chat_scope(&ctx, "send_message", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let request_id = input.request_id.unwrap_or_else(Uuid::new_v4);

        // 1. Idempotency (before the parallel-turn guard).
        if let Some(t) = repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id).await? {
            if t.state == repo::STATE_COMPLETED && t.deleted_at.is_none() {
                return Ok(StreamStart::Replay(self.replay_events(&scopes.tenant, &chat, &t).await?));
            }
            return Err(request_id_conflict());
        }
        // 2. Parallel-turn guard.
        if repo::running_turn(&conn, &scopes.tenant, chat_id).await?.is_some() {
            return Err(turn_already_running());
        }
        drop(conn);
        // 3. Preflight, context assembly, provider resolution.
        let pre = self
            .preflight(&ctx, &chat, &scopes.tenant, &input.content, &input.attachment_ids, input.web_search)
            .await?;
        let assembled = self
            .assemble_request(&ctx, &chat, &scopes.tenant, &pre, &input.content, None)
            .await?;

        // 4. Reserve transaction.
        let turn_id = Uuid::new_v4();
        let user_msg_id = Uuid::new_v4();
        let res = self
            .reserve_and_create_turn(&ctx, &scopes, &chat, &pre, &input, request_id, turn_id, user_msg_id)
            .await;
        if let Err(e) = res {
            if e.is_unique_violation() {
                let conn = self.db.conn()?;
                if repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id).await?.is_some() {
                    return Err(request_id_conflict());
                }
                return Err(turn_already_running());
            }
            return Err(e);
        }
        let run = self.turn_run(&ctx, &chat, &pre, assembled, turn_id, request_id, true);
        Ok(StreamStart::Live(self.spawn_run(run)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn reserve_and_create_turn(
        &self,
        ctx: &SecurityContext,
        scopes: &ChatScopes,
        chat: &chats::Model,
        pre: &Preflight,
        input: &SendInput,
        request_id: Uuid,
        turn_id: Uuid,
        user_msg_id: Uuid,
    ) -> DomainResult<()> {
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let tier = pre.decision.effective.tier;
        let periods = pre.periods;
        let reserved = pre.decision.reserve.reserved_credits_micro;
        let limits = pre.limits.clone();
        let scopes = scopes.clone();
        let chat_id = chat.id;
        let content = input.content.clone();
        let attachment_ids = input.attachment_ids.clone();
        let web_search = input.web_search;
        let effective_model = pre.decision.effective.id.clone();
        let reserve = pre.decision.reserve;
        let floor = pre.floor;
        let policy_version = i64::try_from(pre.snap.policy_version).unwrap_or(i64::MAX);
        self
            .write_tx(move |tx| {
                let scopes = scopes.clone();
                let limits = limits.clone();
                let content = content.clone();
                let attachment_ids = attachment_ids.clone();
                let effective_model = effective_model.clone();
                Box::pin(async move {
                    if !book_reserve(tx, tenant_id, user_id, tier, periods, reserved, &limits).await? {
                        return Err(DomainError::quota_exceeded("tokens"));
                    }
                    let now = repo::now();
                    repo::insert_message(
                        tx,
                        &scopes.tenant,
                        NewMessage {
                            id: user_msg_id,
                            tenant_id,
                            chat_id,
                            request_id,
                            role: "user",
                            content,
                            model: None,
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: 0,
                            provider_response_id: None,
                            created_at: now,
                        },
                    )
                    .await?;
                    repo::touch_chat(tx, &scopes.tenant, chat_id, now).await?;
                    validate_and_link_attachments(tx, &scopes.tenant, tenant_id, user_id, chat_id, user_msg_id, &attachment_ids, now)
                        .await?;
                    repo::insert_turn(
                        tx,
                        &scopes.tenant,
                        &NewTurn {
                            id: turn_id,
                            tenant_id,
                            chat_id,
                            request_id,
                            requester_user_id: user_id,
                            web_search_enabled: web_search,
                            started_at: now,
                        },
                        Some(PreflightCols {
                            reserve_tokens: reserve.reserve_tokens,
                            max_output_tokens_applied: reserve.max_output_tokens_applied,
                            reserved_credits_micro: reserve.reserved_credits_micro,
                            policy_version_applied: policy_version,
                            effective_model: &effective_model,
                            minimal_generation_floor_applied: floor,
                        }),
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn turn_run(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        pre: &Preflight,
        assembled: Assembled,
        turn_id: Uuid,
        request_id: Uuid,
        has_reserve: bool,
    ) -> TurnRun {
        let eff = pre.decision.effective.clone();
        TurnRun {
            ctx: ctx.clone(),
            chat_id: chat.id,
            turn_id,
            request_id,
            assistant_message_id: Uuid::new_v4(),
            selected_model: chat.model.clone(),
            billing: TurnBilling {
                tenant_id: ctx.subject_tenant_id(),
                user_id: Some(ctx.subject_id()),
                chat_id: chat.id,
                turn_id,
                request_id,
                selected_model: chat.model.clone(),
                effective_model: eff.id.clone(),
                tier: eff.tier,
                policy_version: pre.snap.policy_version,
                reserve_tokens: pre.decision.reserve.reserve_tokens,
                max_output_tokens_applied: pre.decision.reserve.max_output_tokens_applied,
                reserved_credits_micro: pre.decision.reserve.reserved_credits_micro,
                floor: pre.floor,
                periods: pre.periods,
                in_mult: eff.input_tokens_credit_multiplier_micro,
                out_mult: eff.output_tokens_credit_multiplier_micro,
                has_reserve,
            },
            effective: eff,
            decision: pre.decision.clone(),
            assembled,
        }
    }

    /// Spawn the provider task for a committed running turn.
    pub(crate) fn spawn_run(self: &Arc<Self>, run: TurnRun) -> LiveStream {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let state = Arc::clone(self);
        let c = cancel.clone();
        tokio::spawn(async move {
            state.run_turn(run, tx, c).await;
        });
        LiveStream { rx, cancel }
    }
}

/// Validate `attachment_ids` against the chat and insert the links.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_and_link_attachments(
    tx: &impl toolkit_db::secure::DBRunner,
    tenant_scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    now: time::OffsetDateTime,
) -> DomainResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let rows = repo::attachments_by_ids(tx, tenant_scope, ids).await?;
    for id in ids {
        let ok = rows.iter().any(|a| {
            a.id == *id
                && a.tenant_id == tenant_id
                && a.chat_id == chat_id
                && a.uploaded_by_user_id == user_id
                && a.status == "ready"
                && a.deleted_at.is_none()
        });
        if !ok {
            return Err(DomainError::invalid_attachment(format!("attachment {id} is not available")));
        }
    }
    repo::insert_message_attachments(tx, tenant_scope, tenant_id, chat_id, message_id, ids, now).await
}

enum Outcome {
    Completed(crate::infra::llm::Completion),
    Failed(crate::infra::llm::LlmFailure),
    Limit(&'static str, String),
    Cancelled,
}

fn snippet_of(text: Option<&str>, part: Option<&str>, start: Option<usize>, end: Option<usize>) -> String {
    if let Some(t) = text.filter(|t| !t.is_empty()) {
        return t.to_owned();
    }
    match (part, start, end) {
        (Some(p), Some(s), Some(e)) if e >= s => {
            let chars: Vec<char> = p.chars().collect();
            if e <= chars.len() { chars[s..e].iter().collect() } else { String::new() }
        }
        _ => String::new(),
    }
}

fn map_citations(raw: &[RawCitation], file_map: &HashMap<String, (Uuid, String)>, final_text: &str) -> Vec<CitationView> {
    let mut out = Vec::new();
    for c in raw {
        match c {
            RawCitation::Url { url, title, start, end, text, part_text } => {
                let part = part_text.as_deref().filter(|p| !p.is_empty()).or(Some(final_text));
                out.push(CitationView {
                    source: "web",
                    title: title.clone(),
                    url: Some(url.clone()),
                    attachment_id: None,
                    snippet: snippet_of(text.as_deref(), part, *start, *end),
                    span: match (start, end) {
                        (Some(s), Some(e)) => Some((*s, *e)),
                        _ => None,
                    },
                });
            }
            RawCitation::File { file_id, start, end, .. } => {
                if let Some((aid, filename)) = file_map.get(file_id) {
                    out.push(CitationView {
                        source: "file",
                        title: filename.clone(),
                        url: None,
                        attachment_id: Some(*aid),
                        snippet: String::new(),
                        span: match (start, end) {
                            (Some(s), Some(e)) => Some((*s, *e)),
                            _ => None,
                        },
                    });
                }
            }
        }
    }
    out
}

impl AppState {
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run_turn(self: Arc<Self>, run: TurnRun, tx: mpsc::Sender<StreamEvent>, cancel: CancellationToken) {
        let started = Instant::now();
        let tenant_scope = AccessScope::for_tenant(run.billing.tenant_id);
        let ping_every = Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds));
        let started_ev = StreamEvent::StreamStarted {
            request_id: run.request_id,
            message_id: run.assistant_message_id,
            is_new_turn: true,
            thread_summary_tokens: run.assembled.summary_tokens,
        };
        let mut disconnected = tx.send(started_ev).await.is_err();

        let mut text = String::new();
        let mut citations: Vec<RawCitation> = Vec::new();
        let mut ws_started = 0_u32;
        let mut ci_started = 0_u32;
        let mut ws_done = 0_i32;
        let mut ci_done = 0_i32;
        let mut fs_done = 0_i32;
        let mut ttft: Option<u64> = None;
        let mut content_started = false;
        let mut last_progress = Instant::now();
        let q = self.cfg.quota.clone();

        let outcome = if disconnected {
            Outcome::Cancelled
        } else {
            // Open the provider stream, pinging while waiting.
            let open = self.llm.stream(&run.assembled.target, &run.assembled.request);
            tokio::pin!(open);
            let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
            let opened = loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break None,
                    r = &mut open => break Some(r),
                    _ = ping.tick() => {
                        if tx.send(StreamEvent::Ping).await.is_err() { disconnected = true; break None; }
                    }
                }
            };
            match opened {
                None => Outcome::Cancelled,
                Some(Err(f)) => Outcome::Failed(f),
                Some(Ok(mut provider)) => {
                let mut request = run.assembled.request.clone();
                let mut pending_calls: Vec<(String, String, String)> = Vec::new();
                let mut knowledge_calls = 0_u32;
                let mut iterations = 1_u32;
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break Outcome::Cancelled,
                        ev = provider.next() => {
                            let Some(ev) = ev else {
                                break Outcome::Failed(crate::infra::llm::LlmFailure::provider("Provider stream ended without a terminal event"));
                            };
                            match ev {
                                LlmEvent::TextDelta(d) | LlmEvent::ReasoningDelta(d) if d.is_empty() => {}
                                LlmEvent::TextDelta(d) => {
                                    if ttft.is_none() { ttft = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)); }
                                    content_started = true;
                                    text.push_str(&d);
                                    if tx.send(StreamEvent::Delta { kind: "text", content: d }).await.is_err() {
                                        disconnected = true;
                                        break Outcome::Cancelled;
                                    }
                                }
                                LlmEvent::ReasoningDelta(d) => {
                                    content_started = true;
                                    if tx.send(StreamEvent::Delta { kind: "reasoning", content: d }).await.is_err() {
                                        disconnected = true;
                                        break Outcome::Cancelled;
                                    }
                                }
                                LlmEvent::ToolStart { name, details } => {
                                    if name == "web_search" {
                                        ws_started += 1;
                                        if ws_started > q.web_search_max_calls_per_message {
                                            break Outcome::Limit("web_search_calls_exceeded", "Web search call limit per message exceeded".to_owned());
                                        }
                                    }
                                    if name == "code_interpreter" {
                                        ci_started += 1;
                                        if ci_started > q.code_interpreter_max_calls_per_message {
                                            break Outcome::Limit("code_interpreter_calls_exceeded", "Code interpreter call limit per message exceeded".to_owned());
                                        }
                                    }
                                    content_started = true;
                                    if tx.send(StreamEvent::Tool { phase: "start", name, details }).await.is_err() {
                                        disconnected = true;
                                        break Outcome::Cancelled;
                                    }
                                }
                                LlmEvent::ToolDone { name, details } => {
                                    match name.as_str() {
                                        "web_search" => ws_done += 1,
                                        "code_interpreter" => ci_done += 1,
                                        "file_search" => fs_done += 1,
                                        _ => {}
                                    }
                                    content_started = true;
                                    if let Ok(conn) = self.db.conn() {
                                        let _ = repo::update_turn_progress(&conn, &tenant_scope, run.turn_id, repo::now(), (ws_done, ci_done, fs_done)).await;
                                    }
                                    last_progress = Instant::now();
                                    if tx.send(StreamEvent::Tool { phase: "done", name, details }).await.is_err() {
                                        disconnected = true;
                                        break Outcome::Cancelled;
                                    }
                                }
                                LlmEvent::Citation(c) => citations.push(c),
                                LlmEvent::FunctionCall { name, call_id, arguments } => {
                                    if run.assembled.knowledge.is_some() && name == crate::infra::llm::KNOWLEDGE_TOOL {
                                        pending_calls.push((name, call_id, arguments));
                                    } else {
                                        tracing::warn!(tool = %name, "unexpected function tool use");
                                        break Outcome::Limit("unexpected_tool_use", "The model requested a tool that is not available".to_owned());
                                    }
                                }
                                LlmEvent::Completed(c) => {
                                    let Some(k) = run.assembled.knowledge.clone() else { break Outcome::Completed(c) };
                                    if pending_calls.is_empty() {
                                        break Outcome::Completed(c);
                                    }
                                    iterations += 1;
                                    if iterations > self.cfg.knowledge_search.max_calls_per_message + 2 {
                                        break Outcome::Limit("agentic_iterations_exceeded", "Tool iteration limit exceeded".to_owned());
                                    }
                                    for (name, call_id, arguments) in pending_calls.drain(..) {
                                        knowledge_calls += 1;
                                        let (output, ok) = self.run_knowledge_call(&k, &arguments, knowledge_calls).await;
                                        if ok {
                                            fs_done += 1;
                                        }
                                        request.extra_input.push(json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": arguments}));
                                        request.extra_input.push(json!({"type": "function_call_output", "call_id": call_id, "output": output}));
                                    }
                                    match self.llm.stream(&run.assembled.target, &request).await {
                                        Ok(next) => provider = next,
                                        Err(f) => break Outcome::Failed(f),
                                    }
                                }
                                LlmEvent::Failed(f) => break Outcome::Failed(f),
                            }
                            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                                last_progress = Instant::now();
                                if let Ok(conn) = self.db.conn() {
                                    let _ = repo::update_turn_progress(&conn, &tenant_scope, run.turn_id, repo::now(), (ws_done, ci_done, fs_done)).await;
                                }
                            }
                        }
                        _ = tokio::time::sleep(ping_every), if !content_started => {
                            if tx.send(StreamEvent::Ping).await.is_err() {
                                disconnected = true;
                                break Outcome::Cancelled;
                            }
                        }
                    }
                }
                }
            }
        };
        let _ = disconnected;

        let total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut input = FinalizeInput {
            billing: run.billing.clone(),
            state: repo::STATE_FAILED,
            error_code: None,
            error_detail: None,
            usage: None,
            provider_response_id: None,
            assistant_message: None,
            web_search_calls: ws_done,
            code_interpreter_calls: ci_done,
            file_search_calls: fs_done,
            quota_decision: run.decision.quota_decision().to_owned(),
            downgrade_reason: run.decision.downgrade_reason.clone(),
            ttft_ms: ttft,
            total_ms,
            summary: None,
            orphan_cutoff: None,
        };
        let assistant = |content: String, usage: Option<Usage>, rid: Option<String>| NewMessage {
            id: run.assistant_message_id,
            tenant_id: run.billing.tenant_id,
            chat_id: run.chat_id,
            request_id: run.request_id,
            role: "assistant",
            content,
            model: Some(run.effective.id.clone()),
            input_tokens: usage.map_or(0, |u| u.input_tokens.max(0)),
            output_tokens: usage.map_or(0, |u| u.output_tokens.max(0)),
            cache_read_input_tokens: usage.map_or(0, |u| u.cache_read_input_tokens.max(0)),
            cache_write_input_tokens: usage.map_or(0, |u| u.cache_write_input_tokens.max(0)),
            reasoning_tokens: usage.map_or(0, |u| u.reasoning_tokens.max(0)),
            provider_response_id: rid,
            created_at: repo::now(),
        };

        match outcome {
            Outcome::Completed(c) => {
                if let Some(r) = &c.incomplete_reason {
                    tracing::warn!(reason = %r, "stream incomplete");
                }
                input.state = repo::STATE_COMPLETED;
                input.usage = c.usage;
                input.provider_response_id.clone_from(&c.response_id);
                input.assistant_message = Some(assistant(text.clone(), c.usage, c.response_id.clone()));
                input.summary = self.summary_trigger(&run).await;
                match self.finalize_turn(input.clone()).await {
                    Ok(FinalizeOutcome::Won) => {
                        let mapped = map_citations(&citations, &run.assembled.file_map, &text);
                        if !mapped.is_empty() {
                            let _ = tx.send(StreamEvent::Citations(mapped)).await;
                        }
                        let warnings = self.quota_warnings(&run).await;
                        let u = c.usage.unwrap_or_default();
                        let _ = tx
                            .send(StreamEvent::Done(DoneView {
                                input_tokens: u.input_tokens,
                                output_tokens: u.output_tokens,
                                effective_model: run.effective.id.clone(),
                                selected_model: run.selected_model.clone(),
                                downgraded: run.decision.downgraded,
                                downgrade_reason: run.decision.downgrade_reason.clone(),
                                quota_warnings: warnings,
                            }))
                            .await;
                    }
                    Ok(FinalizeOutcome::Lost) => {
                        let _ = tx.send(stream_interrupted()).await;
                    }
                    Err(e) if e.to_string().contains(MESSAGE_PERSISTENCE_MARKER) => {
                        tracing::error!(error = %e, "assistant message persistence failed");
                        let mut f = input.clone();
                        f.state = repo::STATE_FAILED;
                        f.error_code = Some("message_persistence_failed".to_owned());
                        f.assistant_message = None;
                        f.summary = None;
                        let _ = self.finalize_turn(f).await;
                        let _ = tx
                            .send(StreamEvent::Error {
                                code: "message_persistence_failed".to_owned(),
                                message: "The response could not be saved".to_owned(),
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "finalization failed");
                        let _ = tx
                            .send(StreamEvent::Error {
                                code: "finalization_failed".to_owned(),
                                message: "The response could not be finalized".to_owned(),
                            })
                            .await;
                    }
                }
            }
            Outcome::Failed(f) => {
                input.state = repo::STATE_FAILED;
                input.error_code = Some(f.code.to_owned());
                input.error_detail = Some(f.message.clone());
                input.usage = f.usage;
                match self.finalize_turn(input).await {
                    Ok(FinalizeOutcome::Lost) => {
                        let _ = tx.send(stream_interrupted()).await;
                    }
                    Ok(FinalizeOutcome::Won) | Err(_) => {
                        let _ = tx
                            .send(StreamEvent::Error {
                                code: f.code.to_owned(),
                                message: f.message,
                            })
                            .await;
                    }
                }
            }
            Outcome::Limit(code, message) => {
                input.state = repo::STATE_FAILED;
                input.error_code = Some(code.to_owned());
                input.error_detail = Some(message.clone());
                match self.finalize_turn(input).await {
                    Ok(FinalizeOutcome::Lost) => {
                        let _ = tx.send(stream_interrupted()).await;
                    }
                    _ => {
                        let _ = tx
                            .send(StreamEvent::Error {
                                code: code.to_owned(),
                                message,
                            })
                            .await;
                    }
                }
            }
            Outcome::Cancelled => {
                input.state = repo::STATE_CANCELLED;
                if !text.is_empty() {
                    input.assistant_message = Some(assistant(text.clone(), None, None));
                    if self.finalize_turn(input.clone()).await.is_err() {
                        tracing::warn!("partial assistant message could not be persisted");
                        input.assistant_message = None;
                        let _ = self.finalize_turn(input).await;
                    }
                } else if let Err(e) = self.finalize_turn(input).await {
                    tracing::warn!(error = %e, "cancelled turn finalization failed");
                }
            }
        }
    }

    async fn quota_warnings(&self, run: &TurnRun) -> Option<Vec<crate::domain::quota::PeriodStatus>> {
        let user = run.billing.user_id?;
        let limits = self.policy.user_limits(user, run.billing.policy_version).await.ok()?;
        let conn = self.db.conn().ok()?;
        let now = repo::now();
        let usage = repo::read_usage(&conn, run.billing.tenant_id, user, Periods::at(now)).await.ok()?;
        Some(quota_status(&usage, &limits, self.cfg.quota.warning_threshold_pct, now))
    }

    /// Evaluate the thread-summary trigger for a completed turn.
    async fn summary_trigger(&self, run: &TurnRun) -> Option<ThreadSummaryPayload> {
        if !self.cfg.thread_summary_worker.enabled {
            return None;
        }
        let t = &run.assembled.summary_trigger;
        let threshold = t.effective_budget.saturating_mul(i64::from(self.cfg.thread_summary_worker.compression_threshold_pct)) / 100;
        let proactive = !t.summary_exists && t.assembled_tokens >= threshold;
        if !(proactive || t.messages_truncated) {
            return None;
        }
        let scope = AccessScope::for_tenant(run.billing.tenant_id);
        let conn = self.db.conn().ok()?;
        let msgs: Vec<messages::Model> = repo::list_live_messages(&conn, &scope, run.chat_id).await.ok()?;
        let target = msgs
            .iter()
            .rev()
            .find(|m| m.request_id != Some(run.request_id) && m.role != "system")?;
        if let Some((fc, fid)) = t.frontier
            && (target.created_at, target.id) <= (fc, fid)
        {
            return None;
        }
        Some(ThreadSummaryPayload {
            tenant_id: run.billing.tenant_id,
            chat_id: run.chat_id,
            system_request_id: Uuid::new_v4(),
            base_frontier_created_at: t.frontier.map(|f| f.0),
            base_frontier_message_id: t.frontier.map(|f| f.1),
            frozen_target_created_at: target.created_at,
            frozen_target_message_id: target.id,
            system_task_type: "thread_summary_update".to_owned(),
        })
    }
}

fn stream_interrupted() -> StreamEvent {
    StreamEvent::Error {
        code: "stream_interrupted".to_owned(),
        message: "The stream was interrupted".to_owned(),
    }
}

#[allow(dead_code)]
fn _tier_used(t: ModelTier) -> serde_json::Value {
    json!(t.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_ranges() {
        assert_eq!(snippet_of(None, Some("hello world"), Some(0), Some(5)), "hello");
        assert_eq!(snippet_of(None, Some("hi"), Some(0), Some(5)), "");
        assert_eq!(snippet_of(Some("x"), Some("hi"), Some(0), Some(1)), "x");
    }

    #[test]
    fn citations_map_and_filter() {
        let mut fm = HashMap::new();
        fm.insert("file-abc".to_owned(), (Uuid::nil(), "doc.pdf".to_owned()));
        let raw = vec![
            RawCitation::File { file_id: "file-abc".into(), filename: "x".into(), start: None, end: None },
            RawCitation::File { file_id: "file-unknown".into(), filename: "y".into(), start: None, end: None },
            RawCitation::Url { url: "https://u".into(), title: "T".into(), start: Some(0), end: Some(2), text: None, part_text: None },
        ];
        let out = map_citations(&raw, &fm, "Hello");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "doc.pdf");
        assert_eq!(out[1].snippet, "He");
        assert_eq!(out[1].span, Some((0, 2)));
    }
}
