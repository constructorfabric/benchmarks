//! Send-message pipeline: checks, preflight, reserve, provider task (DESIGN §3.6).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use mini_chat_sdk::{PolicySnapshot, UserLimits};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DbTx, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::infra::db::WriteTransaction as _;
use crate::domain::authz::actions;
use crate::domain::context::{self, ContextInputs, ContextPlan, HistoryMessage, ImageRef};
use crate::domain::credits::{ReserveInputs, ToolFlags, estimate_text_tokens};
use crate::domain::error::DomainError;
use crate::domain::events::{CitationItem, Done, DoneUsage, Span, StreamEvent, StreamStarted, ThreadSummaryApplied};
use crate::domain::finalization::{Outcome, TurnCtx};
use crate::domain::quota::{self, CascadeRequest, PeriodStarts, PreflightDecision};
use crate::domain::repo::{self, state};
use crate::domain::service::{Svc, child_scope};
use crate::infra::db::entities::{attachments, chat_turns, chats, message_attachments, messages};
use crate::infra::db::now;
use crate::infra::llm::{LlmRequest, ProviderEvent, RawCitation, Role, ToolSpec};

/// `messages:stream` request.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    /// Content.
    pub content: String,
    /// Client request id.
    pub request_id: Option<Uuid>,
    /// Attachments.
    pub attachment_ids: Vec<Uuid>,
    /// `web_search.enabled`.
    pub web_search: bool,
}

/// Replay of a completed turn.
#[derive(Debug, Clone)]
pub struct ReplayData {
    /// `stream_started`.
    pub started: StreamStarted,
    /// Stored text.
    pub text: String,
    /// `done`.
    pub done: Done,
}

/// Result of stream setup.
pub enum StreamStart {
    /// Replay of a completed turn.
    Replay(Box<ReplayData>),
    /// New generation.
    Live(Box<TurnCtx>),
}

/// Chat attachment facts used by preflight.
#[derive(Debug, Clone, Default)]
pub struct ChatFiles {
    /// Ready attachments.
    pub ready: Vec<attachments::Model>,
    /// Chat vector store id.
    pub vector_store_id: Option<String>,
}

impl ChatFiles {
    fn has_ready_documents(&self) -> bool {
        self.vector_store_id.is_some()
            && self.ready.iter().any(|a| a.attachment_kind == "document" && a.for_file_search)
    }

    fn code_interpreter_files(&self) -> Vec<String> {
        self.ready
            .iter()
            .filter(|a| a.for_code_interpreter)
            .filter_map(|a| a.provider_file_id.clone())
            .collect()
    }
}

/// Output of preflight part A (cascade and guards).
pub struct PreflightA {
    /// Policy snapshot.
    pub snapshot: PolicySnapshot,
    /// User limits.
    pub limits: UserLimits,
    /// Cascade decision.
    pub decision: PreflightDecision,
    /// Period starts.
    pub starts: PeriodStarts,
    /// Timestamp the period starts were computed from.
    pub ts: OffsetDateTime,
}

fn user_field(tenant: Uuid, user: Uuid) -> String {
    format!("{}{}", tenant.simple(), user.simple())
}

fn feature_label(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return "none".to_owned();
    }
    tools.iter().map(ToolSpec::name).collect::<Vec<_>>().join("+")
}

impl Svc {
    pub(crate) async fn chat_files(&self, scope: &AccessScope, chat_id: Uuid) -> Result<ChatFiles, DomainError> {
        let conn = self.db.conn()?;
        let ready = repo::ready_attachments(&conn, scope, chat_id).await?;
        let vector_store_id = repo::chat_vector_store(&conn, scope, chat_id).await?.and_then(|v| v.vector_store_id);
        Ok(ChatFiles { ready, vector_store_id })
    }

    /// Validates `attachment_ids` of a message (DESIGN "Attachment Preflight Validation").
    pub(crate) async fn validate_attachments(
        &self,
        scope: &AccessScope,
        chat: &chats::Model,
        user_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<attachments::Model>, DomainError> {
        let max = (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if ids.len() > max {
            return Err(DomainError::InvalidAttachment("too many attachment_ids".into()));
        }
        let mut seen = std::collections::HashSet::new();
        if !ids.iter().all(|id| seen.insert(*id)) {
            return Err(DomainError::InvalidAttachment("duplicate attachment_ids".into()));
        }
        let conn = self.db.conn()?;
        let found = repo::attachments_by_ids(&conn, scope, chat.id, ids).await?;
        if found.len() != ids.len()
            || found.iter().any(|a| a.uploaded_by_user_id != user_id || a.status != "ready")
        {
            return Err(DomainError::InvalidAttachment("attachment is unknown, foreign or not ready".into()));
        }
        Ok(found)
    }

    /// Preflight part A: snapshot, cascade, tool quotas, input limit, image guards.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn preflight_a(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        content: &str,
        images: usize,
        web_search: bool,
        files: &ChatFiles,
        prior_tokens: i64,
    ) -> Result<PreflightA, DomainError> {
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        if snapshot.model(&chat.model).is_none() {
            return Err(DomainError::InvalidModel);
        }
        let ks = snapshot.kill_switches;
        if web_search && ks.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        if images > self.cfg.rag.max_images_per_message as usize {
            return Err(DomainError::TooManyImages { limit: self.cfg.rag.max_images_per_message });
        }
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record("image_inputs_per_turn", images as f64, &[]);
        let limits = self.policy.user_limits(ctx.subject_id(), snapshot.policy_version).await?;
        let ts = now();
        let starts = PeriodStarts::of(ts);
        let conn = self.db.conn()?;
        let rows = quota::read_rows(&conn, ctx.subject_tenant_id(), ctx.subject_id(), &starts).await?;
        let req = CascadeRequest {
            reserve_inputs: ReserveInputs {
                message_bytes: content.len(),
                prior_context_tokens: prior_tokens,
                image_count: u32::try_from(images).unwrap_or(u32::MAX),
                tools: ToolFlags::default(),
            },
            eligible_tools: ToolFlags {
                file_search: files.has_ready_documents(),
                web_search,
                code_interpreter: !files.code_interpreter_files().is_empty(),
            },
            streaming_max_output_tokens: self.cfg.streaming.max_output_tokens,
        };
        let decision = match quota::cascade(&snapshot, &limits, &rows, &chat.model, &req) {
            Ok(d) => d,
            Err(e) => {
                self.metrics.inc("quota_preflight_total", &[("decision", "reject"), ("model", &chat.model), ("tier", "none")]);
                return Err(e);
            }
        };
        quota::check_tool_quotas(
            &rows,
            decision.tools,
            self.cfg.quota.web_search_daily_quota,
            self.cfg.quota.code_interpreter_daily_quota,
        )?;
        self.metrics.inc(
            "quota_preflight_total",
            &[("decision", decision.decision.as_str()), ("model", &decision.effective.id), ("tier", decision.effective.tier.as_str())],
        );
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record("quota_estimated_tokens", decision.reserve.reserve_tokens as f64, &[]);
        let eff = &decision.effective;
        if eff.max_input_tokens > 0
            && estimate_text_tokens(content.len(), &eff.estimation_budgets) > i64::from(eff.max_input_tokens)
        {
            return Err(DomainError::InputTooLong);
        }
        if images > 0 {
            if ks.disable_images {
                return Err(DomainError::FeatureDisabled("images"));
            }
            if !eff.supports_vision() {
                return Err(DomainError::VisionNotSupported);
            }
        }
        Ok(PreflightA { snapshot, limits, decision, starts, ts })
    }

    /// Preflight part B: context assembly, provider resolution, request build.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn preflight_b(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
        scope: &AccessScope,
        content: &str,
        images: &[attachments::Model],
        files: &ChatFiles,
        a: &PreflightA,
        exclude_request: Option<Uuid>,
    ) -> Result<(ContextPlan, LlmRequest, crate::infra::llm::resolver::ChatTarget, bool), DomainError> {
        let eff = &a.decision.effective;
        let conn = self.db.conn()?;
        let summary = repo::thread_summary(&conn, scope, chat.id).await?;
        let frontier = summary.as_ref().map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let recent = repo::recent_messages(
            &conn,
            scope,
            chat.id,
            frontier,
            exclude_request,
            u64::from(self.cfg.context.recent_messages_limit),
        )
        .await?;
        let recent: Vec<HistoryMessage> = recent
            .into_iter()
            .map(|m| HistoryMessage {
                role: if m.role == "assistant" { Role::Assistant } else { Role::User },
                content: m.content,
            })
            .collect();
        let image_refs: Vec<ImageRef> = images
            .iter()
            .filter_map(|i| {
                i.provider_file_id.clone().map(|f| ImageRef { file_id: f, secondary_file_id: i.secondary_file_id.clone() })
            })
            .collect();
        let max_out = u32::try_from(a.decision.reserve.max_output_tokens_applied).unwrap_or(u32::MAX);
        let plan = context::assemble(&ContextInputs {
            model: eff,
            max_output_tokens_applied: max_out,
            tools: a.decision.tools,
            knowledge_search: false,
            guards: (
                &self.cfg.context.file_search_guard,
                &self.cfg.context.web_search_guard,
                &self.cfg.knowledge_search.guard,
            ),
            summary: summary.as_ref().map(|s| (s.summary_text.clone(), s.token_estimate)),
            recent,
            current_text: content,
            current_images: image_refs,
        })?;
        let target = self.llm.resolver.chat_target(&eff.provider_id, ctx.subject_tenant_id())?;
        let mut tools = Vec::new();
        if a.decision.tools.file_search
            && let Some(vs) = &files.vector_store_id
        {
            tools.push(ToolSpec::FileSearch { vector_store_ids: vec![vs.clone()], max_num_results: eff.max_num_results });
        }
        if a.decision.tools.web_search {
            tools.push(ToolSpec::WebSearch { context_size: eff.web_search_context_size });
        }
        if a.decision.tools.code_interpreter {
            tools.push(ToolSpec::CodeInterpreter { file_ids: files.code_interpreter_files() });
        }
        let mut metadata = Map::new();
        metadata.insert("tenant_id".into(), json!(ctx.subject_tenant_id().to_string()));
        metadata.insert("user_id".into(), json!(ctx.subject_id().to_string()));
        metadata.insert("chat_id".into(), json!(chat.id.to_string()));
        metadata.insert("request_type".into(), json!("chat"));
        metadata.insert("feature".into(), json!(feature_label(&tools)));
        let request = LlmRequest {
            model: eff.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input: plan.input.clone(),
            max_output_tokens: max_out,
            tools,
            max_tool_calls: eff.max_tool_calls,
            user: user_field(ctx.subject_tenant_id(), ctx.subject_id()),
            metadata,
            api_params: eff.general_config.api_params.clone(),
            stream: true,
        };
        Ok((plan, request, target, summary.is_some()))
    }

    /// Builds the replay of a completed turn.
    pub(crate) async fn build_replay(&self, scope: &AccessScope, chat: &chats::Model, turn: &chat_turns::Model) -> Result<ReplayData, DomainError> {
        let conn = self.db.conn()?;
        let msg = match turn.assistant_message_id {
            Some(id) => messages::Entity::find_by_id(id)
                .secure_one(&conn, scope)
                .await?,
            None => None,
        };
        let (text, usage, model, message_id) = match msg {
            Some(m) => (m.content.clone(), DoneUsage { input_tokens: m.input_tokens, output_tokens: m.output_tokens }, m.model.clone(), m.id),
            None => (String::new(), DoneUsage { input_tokens: 0, output_tokens: 0 }, None, turn.assistant_message_id.unwrap_or_else(Uuid::nil)),
        };
        let effective = model.or_else(|| turn.effective_model.clone()).unwrap_or_else(|| chat.model.clone());
        let downgrade = effective != chat.model;
        Ok(ReplayData {
            started: StreamStarted { request_id: turn.request_id, message_id, is_new_turn: false, thread_summary_applied: None },
            text,
            done: Done {
                usage,
                effective_model: effective,
                selected_model: chat.model.clone(),
                quota_decision: if downgrade { "downgrade" } else { "allow" },
                downgrade_from: downgrade.then(|| chat.model.clone()),
                downgrade_reason: None,
                quota_warnings: None,
            },
        })
    }

    /// `POST /chats/{id}/messages:stream` setup (everything before the provider call).
    ///
    /// # Errors
    /// Every pre-stream rejection as a JSON `Problem`.
    pub async fn prepare_send(&self, ctx: &SecurityContext, chat_id: Uuid, req: SendRequest) -> Result<StreamStart, DomainError> {
        let max_ids = (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if req.attachment_ids.len() > max_ids {
            return Err(DomainError::InvalidAttachment("too many attachment_ids".into()));
        }
        let (_, chat) = self.authorized_chat(ctx, actions::SEND_MESSAGE, chat_id).await?;
        let scope = child_scope(&chat);
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        {
            let conn = self.db.conn()?;
            if let Some(turn) = repo::turn_by_request(&conn, &scope, chat_id, request_id).await? {
                if turn.state == state::COMPLETED && turn.deleted_at.is_none() {
                    return Ok(StreamStart::Replay(Box::new(self.build_replay(&scope, &chat, &turn).await?)));
                }
                return Err(DomainError::RequestIdConflict(format!("turn {} in state {}", turn.id, turn.state)));
            }
            if repo::has_running_turn(&conn, &scope, chat_id).await? {
                return Err(DomainError::TurnAlreadyRunning);
            }
        }
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let attached = self.validate_attachments(&scope, &chat, ctx.subject_id(), &req.attachment_ids).await?;
        let images: Vec<attachments::Model> = attached.iter().filter(|a| a.attachment_kind == "image").cloned().collect();
        let files = self.chat_files(&scope, chat_id).await?;
        let prior = {
            let conn = self.db.conn()?;
            repo::prior_context_tokens(&conn, &scope, chat_id).await?
        };
        let a = self.preflight_a(ctx, &chat, &req.content, images.len(), req.web_search, &files, prior).await?;
        let (plan, request, target, has_summary) =
            self.preflight_b(ctx, &chat, &scope, &req.content, &images, &files, &a, None).await?;

        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let assistant_message_id = Uuid::new_v4();
        let ts = a.ts;
        let floor = i64::from(self.cfg.estimation_budgets.minimal_generation_floor).min(a.decision.reserve.max_output_tokens_applied);
        let tenant_id = chat.tenant_id;
        let user_id = ctx.subject_id();
        let tier = a.decision.effective.tier;
        let reserve = a.decision.reserve;
        let limits = a.limits.clone();
        let starts = a.starts;
        let content = req.content.clone();
        let ids = req.attachment_ids.clone();
        let tx_scope = scope.clone();
        let effective_id = a.decision.effective.id.clone();
        let policy_version = a.snapshot.policy_version;
        let web_search = req.web_search;
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    quota::write_reserve(tx, tenant_id, user_id, &starts, tier, reserve.reserved_credits_micro, &limits).await?;
                    insert_user_message(tx, &tx_scope, tenant_id, chat_id, user_message_id, request_id, &content, ts).await?;
                    touch_chat(tx, &tx_scope, chat_id, ts).await?;
                    if !ids.is_empty() {
                        let found = repo::attachments_by_ids(tx, &tx_scope, chat_id, &ids).await?;
                        if found.len() != ids.len() || found.iter().any(|a| a.status != "ready" || a.uploaded_by_user_id != user_id) {
                            return Err(DomainError::InvalidAttachment("attachment is not ready".into()));
                        }
                        insert_links(tx, &tx_scope, tenant_id, chat_id, user_message_id, &ids, ts).await?;
                    }
                    let am = chat_turns::ActiveModel {
                        id: Set(turn_id),
                        tenant_id: Set(tenant_id),
                        chat_id: Set(chat_id),
                        request_id: Set(request_id),
                        requester_type: Set("user".into()),
                        requester_user_id: Set(Some(user_id)),
                        state: Set(state::RUNNING.into()),
                        provider_name: Set(None),
                        provider_response_id: Set(None),
                        assistant_message_id: Set(None),
                        error_code: Set(None),
                        reserve_tokens: Set(Some(reserve.reserve_tokens)),
                        max_output_tokens_applied: Set(Some(i32::try_from(reserve.max_output_tokens_applied).unwrap_or(i32::MAX))),
                        reserved_credits_micro: Set(Some(reserve.reserved_credits_micro)),
                        policy_version_applied: Set(Some(i64::try_from(policy_version).unwrap_or(i64::MAX))),
                        effective_model: Set(Some(effective_id)),
                        minimal_generation_floor_applied: Set(Some(i32::try_from(floor).unwrap_or(i32::MAX))),
                        error_detail: Set(None),
                        deleted_at: Set(None),
                        replaced_by_request_id: Set(None),
                        started_at: Set(ts),
                        last_progress_at: Set(Some(ts)),
                        web_search_enabled: Set(web_search),
                        web_search_completed_count: Set(0),
                        code_interpreter_completed_count: Set(0),
                        file_search_completed_count: Set(0),
                        completed_at: Set(None),
                        updated_at: Set(ts),
                    };
                    secure_insert::<chat_turns::Entity>(am, &tx_scope, tx).await?;
                    Ok(())
                })
            })
            .await;
        match res {
            Ok(()) => {
                for p in ["daily", "monthly"] {
                    self.metrics.inc("quota_reserve_total", &[("period", p)]);
                }
            }
            Err(DomainError::Conflict(_)) => {
                let conn = self.db.conn()?;
                if repo::turn_by_request(&conn, &scope, chat_id, request_id).await?.is_some() {
                    return Err(DomainError::RequestIdConflict("insert race".into()));
                }
                return Err(DomainError::TurnAlreadyRunning);
            }
            Err(e) => return Err(e),
        }
        let citations = repo::citation_map(&files.ready);
        Ok(StreamStart::Live(Box::new(TurnCtx {
            tenant_id,
            user_id,
            chat_id,
            turn_id,
            request_id,
            assistant_message_id,
            user_message_id,
            user_message_created_at: ts,
            selected_model: chat.model.clone(),
            effective: a.decision.effective.clone(),
            decision: a.decision.decision,
            downgrade_reason: a.decision.downgrade_reason,
            reserve,
            policy_version,
            limits: a.limits,
            starts: a.starts,
            floor_applied: floor,
            tools: a.decision.tools,
            target,
            request,
            citations,
            assembled_tokens: plan.assembled_tokens,
            effective_budget: plan.effective_budget,
            messages_truncated: plan.messages_truncated,
            has_summary,
            summary_applied: plan.summary_applied,
            started: Instant::now(),
        })))
    }

    /// `stream_started` of a live turn.
    #[must_use]
    pub fn stream_started(turn: &TurnCtx) -> StreamStarted {
        StreamStarted {
            request_id: turn.request_id,
            message_id: turn.assistant_message_id,
            is_new_turn: true,
            thread_summary_applied: turn
                .summary_applied
                .map(|t| ThreadSummaryApplied { token_estimate: u32::try_from(t.max(0)).unwrap_or(0) }),
        }
    }

    fn map_citations(turn: &TurnCtx, raw: Vec<RawCitation>) -> Vec<CitationItem> {
        raw.into_iter()
            .filter_map(|c| match c {
                RawCitation::File { file_id } => turn.citations.get(&file_id).map(|(id, name)| CitationItem {
                    source: "file",
                    title: name.clone(),
                    url: None,
                    attachment_id: Some(*id),
                    snippet: String::new(),
                    span: None,
                }),
                RawCitation::Web { url, title, snippet, span } => Some(CitationItem {
                    source: "web",
                    title,
                    url: Some(url),
                    attachment_id: None,
                    snippet,
                    span: span.map(|(start, end)| Span { start, end }),
                }),
            })
            .collect()
    }

    /// Provider task: streams provider events to the SSE relay and finalizes the turn.
    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "provider stream relay loop: event mapping, tool limits, progress, cancellation and finalization"
    )]
    pub async fn run_turn(self: Arc<Self>, turn: Box<TurnCtx>, tx: mpsc::Sender<StreamEvent>, cancel: CancellationToken) {
        let turn = *turn;
        self.metrics.gauge_add("active_streams", 1);
        self.metrics.inc("stream_started_total", &[("provider", &turn.target.provider_id), ("model", &turn.effective.id)]);
        let scope = AccessScope::for_tenant(turn.tenant_id);
        let mut text = String::new();
        let mut web_started = 0u32;
        let mut ci_started = 0u32;
        let mut counts = (0i32, 0i32, 0i32);
        let mut last_progress = Instant::now();
        let mut first_token: Option<Instant> = None;
        let mut disconnected = false;

        let send = |ev: StreamEvent| {
            let tx = tx.clone();
            let cancel = cancel.clone();
            async move {
                tokio::select! {
                    r = tx.send(ev) => r.is_ok(),
                    () = cancel.cancelled() => false,
                }
            }
        };

        let mut cancel_seen: Option<Instant> = None;
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "ttft_overhead_ms",
            turn.started.elapsed().as_millis() as f64,
            &[("provider", &turn.target.provider_id), ("model", &turn.effective.id)],
        );
        let outcome: Outcome = 'run: {
            let stream = tokio::select! {
                r = self.llm.stream_chat(&turn.target, &turn.request) => r,
                () = cancel.cancelled() => { disconnected = true; cancel_seen = Some(Instant::now()); break 'run Outcome::Cancelled { text: String::new() }; }
            };
            let mut stream = match stream {
                Ok(s) => s,
                Err(e) => break 'run Outcome::Failed { code: e.code.as_str().to_owned(), message: e.message, usage: e.usage },
            };
            loop {
                let ev = tokio::select! {
                    biased;
                    () = cancel.cancelled() => { disconnected = true; cancel_seen = Some(Instant::now()); break 'run Outcome::Cancelled { text: std::mem::take(&mut text) }; }
                    ev = stream.next() => ev,
                };
                let Some(ev) = ev else {
                    break 'run Outcome::Failed { code: "provider_error".into(), message: "Provider stream ended unexpectedly".into(), usage: None };
                };
                let mut progress = false;
                let out = match ev {
                    ProviderEvent::TextDelta(t) => {
                        if first_token.is_none() {
                            first_token = Some(Instant::now());
                            #[allow(clippy::cast_precision_loss)]
                            self.metrics.record("ttft_provider_ms", turn.started.elapsed().as_millis() as f64, &[("provider", &turn.target.provider_id), ("model", &turn.effective.id)]);
                        }
                        text.push_str(&t);
                        progress = true;
                        Some(StreamEvent::Delta { kind: "text", content: t })
                    }
                    ProviderEvent::ReasoningDelta(t) => {
                        progress = true;
                        Some(StreamEvent::Delta { kind: "reasoning", content: t })
                    }
                    ProviderEvent::ToolStart { name, details } => {
                        progress = true;
                        match name.as_str() {
                            "web_search" => {
                                web_started += 1;
                                if web_started > self.cfg.quota.web_search_max_calls_per_message {
                                    break 'run Outcome::Failed {
                                        code: "web_search_calls_exceeded".into(),
                                        message: "Web search call limit per message exceeded".into(),
                                        usage: None,
                                    };
                                }
                            }
                            "code_interpreter" => {
                                ci_started += 1;
                                if ci_started > self.cfg.quota.code_interpreter_max_calls_per_message {
                                    break 'run Outcome::Failed {
                                        code: "code_interpreter_calls_exceeded".into(),
                                        message: "Code interpreter call limit per message exceeded".into(),
                                        usage: None,
                                    };
                                }
                            }
                            "file_search" => {}
                            _ => {
                                break 'run Outcome::Failed {
                                    code: "unexpected_tool_use".into(),
                                    message: "The model requested a tool that is not available".into(),
                                    usage: None,
                                };
                            }
                        }
                        Some(StreamEvent::Tool { phase: "start", name, details })
                    }
                    ProviderEvent::ToolDone { name, details } => {
                        match name.as_str() {
                            "web_search" => counts.0 += 1,
                            "code_interpreter" => counts.1 += 1,
                            "file_search" => counts.2 += 1,
                            _ => {}
                        }
                        if let Ok(conn) = self.db.conn()
                            && let Err(e) = repo::update_progress(&conn, &scope, turn.turn_id, now(), counts).await
                        {
                            tracing::warn!(turn_id = %turn.turn_id, error = %e, "turn progress update failed");
                        }
                        last_progress = Instant::now();
                        Some(StreamEvent::Tool { phase: "done", name, details })
                    }
                    ProviderEvent::Completed { usage, response_id, incomplete_reason, citations } => {
                        let items = Self::map_citations(&turn, citations);
                        if incomplete_reason.is_none() && !items.is_empty() && !send(StreamEvent::Citations(items)).await {
                            disconnected = true;
                            break 'run Outcome::Cancelled { text: std::mem::take(&mut text) };
                        }
                        if let Some(r) = &incomplete_reason {
                            tracing::warn!(reason = %r, "stream incomplete");
                            self.metrics.inc("stream_incomplete_total", &[("provider", &turn.target.provider_id), ("model", &turn.effective.id), ("reason", r)]);
                        }
                        break 'run Outcome::Completed { text: std::mem::take(&mut text), usage, response_id };
                    }
                    ProviderEvent::Failed(e) => {
                        break 'run Outcome::Failed { code: e.code.as_str().to_owned(), message: e.message, usage: e.usage };
                    }
                };
                if progress && last_progress.elapsed() >= Duration::from_secs(30) {
                    if let Ok(conn) = self.db.conn()
                        && let Err(e) = repo::update_progress(&conn, &scope, turn.turn_id, now(), counts).await
                    {
                        tracing::warn!(turn_id = %turn.turn_id, error = %e, "turn progress update failed");
                    }
                    last_progress = Instant::now();
                }
                if let Some(out) = out
                    && !send(out).await
                {
                    disconnected = true;
                    break 'run Outcome::Cancelled { text: std::mem::take(&mut text) };
                }
            }
        };
        if disconnected && let Some(seen) = cancel_seen {
            // The provider stream was dropped with the run block: the HTTP request is aborted.
            #[allow(clippy::cast_precision_loss)]
            self.metrics.record("time_to_abort_ms", seen.elapsed().as_millis() as f64, &[("trigger", "client_disconnect")]);
        }
        let terminal = self.finalize_turn(&turn, outcome, counts, first_token).await;
        if let Some(ev) = terminal
            && tx.send(ev).await.is_err()
        {
            tracing::debug!(turn_id = %turn.turn_id, "terminal event not delivered: SSE relay already closed");
        }
        self.metrics.gauge_add("active_streams", -1);
    }
}

/// Inserts the user message of a turn.
#[allow(clippy::too_many_arguments, reason = "internal insert helper taking the message columns explicitly")]
pub(crate) async fn insert_user_message(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    request_id: Uuid,
    content: &str,
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    let am = messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(Some(request_id)),
        role: Set("user".into()),
        content: Set(content.to_owned()),
        content_type: Set("text".into()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".into()),
        features_used: Set(Value::Array(vec![])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(ts),
        deleted_at: Set(None),
    };
    secure_insert::<messages::Entity>(am, scope, tx).await?;
    Ok(())
}

/// Inserts `message_attachments` links.
pub(crate) async fn insert_links(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    ts: OffsetDateTime,
) -> Result<(), DomainError> {
    for id in ids {
        let am = message_attachments::ActiveModel {
            tenant_id: Set(tenant_id),
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            attachment_id: Set(*id),
            created_at: Set(ts),
        };
        secure_insert::<message_attachments::Entity>(am, scope, tx).await?;
    }
    Ok(())
}

/// Bumps `chats.updated_at`.
pub(crate) async fn touch_chat(tx: &DbTx<'_>, scope: &AccessScope, chat_id: Uuid, ts: OffsetDateTime) -> Result<(), DomainError> {
    chats::Entity::update_many()
        .secure()
        .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
        .scope_with(scope)
        .exec(tx)
        .await?;
    Ok(())
}

/// Helper trait: secure find-by-id.
pub(crate) trait SecureOne<E: EntityTrait> {
    async fn secure_one(self, runner: &impl toolkit_db::secure::DBRunner, scope: &AccessScope) -> Result<Option<E::Model>, DomainError>;
}

impl<E> SecureOne<E> for sea_orm::Select<E>
where
    E: EntityTrait + toolkit_db::secure::ScopableEntity,
    E::Model: Send + Sync,
{
    async fn secure_one(self, runner: &impl toolkit_db::secure::DBRunner, scope: &AccessScope) -> Result<Option<E::Model>, DomainError> {
        use toolkit_db::secure::SecureEntityExt;
        Ok(self.secure().scope_with(scope).one(runner).await?)
    }
}
