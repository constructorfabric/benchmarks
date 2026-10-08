//! Turn setup: idempotency, preflight, context assembly, reserve and turn creation
//! (DESIGN §3.6 "Send Message", "Retry / edit variant").

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{MiniChatAuditEvent, PolicySnapshot, TurnMutationAuditEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::DBRunner;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::citations::FileMap;
use super::events::{Delta, Done, StreamEvent, StreamStarted, ThreadSummaryApplied, UsageOut};
use super::{StreamStart, SummaryTrigger, TurnRuntime};
use crate::domain::app::AppServices;
use crate::domain::authz::actions;
use crate::domain::context::{self, ContextInputs, HistoryMessage};
use crate::domain::credits::estimate_text_tokens;
use crate::domain::error::{
    DisabledFeature, DomainError, DomainResult, QuotaScope, retry_contention,
};
use crate::domain::knowledge::{self, KnowledgeParams};
use crate::domain::quota::{self, Periods, PreflightDecision, PreflightInput, QuotaDecision};
use crate::domain::time::now;
use crate::domain::turns;
use crate::infra::db::entities::{attachment, chat, chat_turn, message};
use crate::infra::db::repo;
use crate::infra::llm::ResolvedProvider;
use crate::infra::llm::responses::{ChatRequest, Role};
use crate::infra::outbox::Wakes;

/// Send-message request (validated `content`).
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Preflight result shared by send and retry/edit.
pub struct Preflight {
    pub snapshot: PolicySnapshot,
    pub decision: PreflightDecision,
    pub image_file_ids: Vec<String>,
    pub web_search_requested: bool,
}

/// Result of context assembly and provider resolution.
pub struct Assembly {
    pub request: ChatRequest,
    pub provider: ResolvedProvider,
    pub files: FileMap,
    pub knowledge: Option<KnowledgeParams>,
    pub summary_trigger: SummaryTrigger,
    pub summary_applied: Option<u32>,
}

/// Content validation (`EMPTY_CONTENT`).
///
/// # Errors
/// `EmptyContent`.
pub fn validate_content(content: &str) -> DomainResult<()> {
    if content.trim().is_empty() {
        return Err(DomainError::EmptyContent);
    }
    Ok(())
}

fn ready(a: &attachment::Model) -> bool {
    a.status == "ready" && a.deleted_at.is_none()
}

impl AppServices {
    /// Resolves the chat's model in the current snapshot (no enabled filter).
    ///
    /// # Errors
    /// `InvalidModel` when the model is no longer in the catalog.
    pub async fn chat_snapshot(
        &self,
        user_id: Uuid,
        chat: &chat::Model,
    ) -> DomainResult<PolicySnapshot> {
        let snapshot = self.policy.current_snapshot(user_id).await?;
        if snapshot.find(&chat.model).is_none() {
            return Err(DomainError::InvalidModel(format!(
                "the chat model '{}' is no longer available",
                chat.model
            )));
        }
        Ok(snapshot)
    }

    /// Preflight: kill switches, cascade, daily tool quotas, input limit, image guards.
    #[allow(clippy::too_many_arguments)]
    async fn preflight_phase(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        snapshot: PolicySnapshot,
        content: &str,
        images: &[attachment::Model],
        web_search: bool,
    ) -> DomainResult<Preflight> {
        let ks = snapshot.kill_switches;
        if web_search && ks.disable_web_search {
            return Err(DomainError::FeatureDisabled(DisabledFeature::WebSearch));
        }
        let conn = self.db.conn()?;
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), snapshot.policy_version)
            .await?;
        let prior = repo::latest_assistant_with_usage(&conn, chat.tenant_id, chat.id)
            .await?
            .map_or(0, |m| m.input_tokens + m.output_tokens);
        let atts = repo::chat_attachments(&conn, chat.tenant_id, chat.id).await?;
        let has_docs = atts
            .iter()
            .any(|a| ready(a) && a.for_file_search && a.attachment_kind == "document");
        let has_xlsx = atts.iter().any(|a| ready(a) && a.for_code_interpreter);
        let periods = Periods::at(now());
        let usage =
            quota::load_usage(&conn, ctx.subject_tenant_id(), ctx.subject_id(), &periods).await?;
        let input = PreflightInput {
            snapshot: &snapshot,
            limits: &limits,
            selected_model: &chat.model,
            message_text: content,
            prior_context_tokens: prior,
            image_count: u32::try_from(images.len()).unwrap_or(u32::MAX),
            chat_has_ready_docs: has_docs,
            chat_has_ready_xlsx: has_xlsx,
            web_search_requested: web_search,
            usage: &usage,
            max_output_cap: self.cfg.streaming.max_output_tokens,
            minimal_generation_floor: self.cfg.estimation_budgets.minimal_generation_floor,
            web_search_daily_quota: self.cfg.quota.web_search_daily_quota,
            code_interpreter_daily_quota: self.cfg.quota.code_interpreter_daily_quota,
            periods,
        };
        let decision = match quota::preflight(&input) {
            Ok(d) => d,
            Err(e) => {
                self.metrics.inc(
                    "quota_preflight",
                    &[
                        ("decision", "reject"),
                        ("model", &chat.model),
                        ("tier", "none"),
                    ],
                );
                return Err(e);
            }
        };
        let decision_label = if decision.decision == QuotaDecision::Allow {
            "allow"
        } else {
            "downgrade"
        };
        let tier_label = if decision.premium {
            "premium"
        } else {
            "standard"
        };
        self.metrics.inc(
            "quota_preflight",
            &[
                ("decision", decision_label),
                ("model", &decision.effective.id),
                ("tier", tier_label),
            ],
        );
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "quota_estimated_tokens",
            decision.reserve_tokens as f64,
            &[],
        );
        let eff = &decision.effective;
        if eff.max_input_tokens > 0
            && estimate_text_tokens(content, &eff.estimation_budgets)
                > i64::from(eff.max_input_tokens)
        {
            return Err(DomainError::InputTooLong);
        }
        if !images.is_empty() {
            if ks.disable_images {
                return Err(DomainError::FeatureDisabled(DisabledFeature::Images));
            }
            if !eff.supports_vision() {
                return Err(DomainError::VisionNotSupported);
            }
        }
        let image_file_ids = images
            .iter()
            .filter_map(|a| a.provider_file_id.clone())
            .collect();
        Ok(Preflight {
            snapshot,
            decision,
            image_file_ids,
            web_search_requested: web_search,
        })
    }

    /// Context assembly, tool construction and provider resolution.
    async fn assembly_phase(
        &self,
        ctx: &SecurityContext,
        chat: &chat::Model,
        pf: &Preflight,
        content: &str,
        boundary: Option<(time::OffsetDateTime, Uuid)>,
    ) -> DomainResult<Assembly> {
        let d = &pf.decision;
        let eff = &d.effective;
        let b = &eff.estimation_budgets;
        let conn = self.db.conn()?;
        let summary = repo::find_summary(&conn, chat.tenant_id, chat.id).await?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let mut recent = repo::recent_messages(
            &conn,
            chat.tenant_id,
            chat.id,
            boundary,
            frontier,
            u64::from(self.cfg.context.recent_messages_limit),
        )
        .await?;
        recent.reverse();
        let history: Vec<HistoryMessage> = recent
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| HistoryMessage {
                role: if m.role == "assistant" {
                    Role::Assistant
                } else {
                    Role::User
                },
                text: m.content.clone(),
            })
            .collect();

        let provider = self.llm.resolve(&eff.provider_id, chat.tenant_id)?;
        let atts = repo::chat_attachments(&conn, chat.tenant_id, chat.id).await?;
        let mut tools = Vec::new();
        let mut guards = Vec::new();
        let mut surcharges = 0_i64;
        let mut files = FileMap::new();
        let mut features = Vec::new();
        if d.tools.file_search {
            let vs = repo::find_vector_store(&conn, chat.tenant_id, chat.id)
                .await?
                .and_then(|v| v.vector_store_id);
            if let Some(vs) = vs {
                tools.push(json!({
                    "type": "file_search",
                    "vector_store_ids": [vs],
                    "max_num_results": eff.max_num_results,
                }));
                guards.push(self.cfg.context.file_search_guard.clone());
                features.push("file_search");
                for a in atts.iter().filter(|a| ready(a)) {
                    if let Some(fid) = &a.provider_file_id {
                        files.insert(fid.clone(), (a.id, a.filename.clone()));
                    }
                }
            }
            surcharges += i64::from(b.tool_surcharge_tokens);
        }
        if d.tools.web_search {
            tools.push(json!({
                "type": "web_search",
                "search_context_size": eff.web_search_context_size.as_str(),
            }));
            guards.push(self.cfg.context.web_search_guard.clone());
            features.push("web_search");
            surcharges += i64::from(b.web_search_surcharge_tokens);
        }
        let mut include_ci = false;
        if d.tools.code_interpreter {
            let file_ids: Vec<String> = atts
                .iter()
                .filter(|a| ready(a) && a.for_code_interpreter)
                .filter_map(|a| a.provider_file_id.clone())
                .collect();
            tools.push(json!({
                "type": "code_interpreter",
                "container": {"type": "auto", "file_ids": file_ids},
            }));
            include_ci = true;
            features.push("code_interpreter");
            surcharges += i64::from(b.code_interpreter_surcharge_tokens);
        }
        // Knowledge search: never together with file_search (file_search wins).
        let knowledge = if d.tools.file_search {
            None
        } else {
            KnowledgeParams::build(&self.cfg.knowledge_search, &self.llm, chat.tenant_id)
        };
        if knowledge.is_some() {
            tools.push(knowledge::tool_definition());
            guards.push(self.cfg.knowledge_search.guard.clone());
            features.push(knowledge::TOOL_NAME);
        }

        let plan = context::assemble(ContextInputs {
            model: eff,
            guards,
            summary: summary.as_ref().map(|s| s.summary_text.clone()),
            history,
            user_text: content,
            image_file_ids: pf.image_file_ids.clone(),
            max_output_tokens_applied: d.max_output_tokens_applied,
            surcharges,
        })?;
        let summary_applied = plan.summary_tokens.map(|_| {
            summary
                .as_ref()
                .map_or(0, |s| u32::try_from(s.token_estimate.max(0)).unwrap_or(0))
        });
        let user = format!(
            "{}{}",
            ctx.subject_tenant_id().as_simple(),
            ctx.subject_id().as_simple()
        );
        let feature = if features.is_empty() {
            "none".to_owned()
        } else {
            features.join("+")
        };
        let request = ChatRequest {
            model: eff.provider_model_id.clone(),
            instructions: plan.instructions.clone(),
            input: plan.messages.clone(),
            max_output_tokens: u32::try_from(d.max_output_tokens_applied).unwrap_or(u32::MAX),
            tools,
            include_code_interpreter_outputs: include_ci,
            max_tool_calls: Some(eff.max_tool_calls),
            user,
            metadata: Some(json!({
                "tenant_id": ctx.subject_tenant_id().to_string(),
                "user_id": ctx.subject_id().to_string(),
                "chat_id": chat.id.to_string(),
                "request_type": "chat",
                "feature": feature,
            })),
            api_params: eff.general_config.api_params.clone(),
            stream: true,
            extra_input: Vec::new(),
        };
        Ok(Assembly {
            request,
            provider,
            files,
            knowledge,
            summary_trigger: SummaryTrigger {
                evaluate: self.cfg.thread_summary_worker.enabled,
                assembled_tokens: plan.assembled_tokens,
                effective_budget: plan.effective_budget,
                truncated: plan.messages_truncated,
                has_summary: summary.is_some(),
            },
            summary_applied,
        })
    }

    /// `POST /chats/{id}/messages:stream` setup.
    ///
    /// # Errors
    /// Every pre-stream error (JSON response, no SSE stream).
    pub async fn start_send(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> DomainResult<StreamStart> {
        validate_content(&req.content)?;
        let chat = self
            .authorized_chat(&ctx, actions::SEND_MESSAGE, chat_id)
            .await?;
        let snapshot = self.chat_snapshot(ctx.subject_id(), &chat).await?;
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let conn = self.db.conn()?;
        if let Some(t) =
            repo::find_turn_by_request(&conn, chat.tenant_id, chat.id, request_id).await?
        {
            if t.state == "completed" && t.deleted_at.is_none() {
                return self.replay(&chat, &t).await;
            }
            return Err(DomainError::RequestIdConflict(format!(
                "turn {} for request {request_id} is {} (deleted: {})",
                t.id,
                t.state,
                t.deleted_at.is_some()
            )));
        }
        if repo::running_turn(&conn, chat.tenant_id, chat.id)
            .await?
            .is_some()
        {
            return Err(DomainError::TurnAlreadyRunning);
        }
        let max_ids = self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message;
        if req.attachment_ids.len() > usize::try_from(max_ids).unwrap_or(usize::MAX) {
            return Err(DomainError::InvalidAttachment(
                "too many attachment_ids".to_owned(),
            ));
        }
        let mut seen = HashSet::new();
        if !req.attachment_ids.iter().all(|id| seen.insert(*id)) {
            return Err(DomainError::InvalidAttachment(
                "duplicate attachment_ids".to_owned(),
            ));
        }
        let atts =
            repo::attachments_by_ids(&conn, chat.tenant_id, chat.id, &req.attachment_ids).await?;
        let images: Vec<attachment::Model> = atts
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .cloned()
            .collect();
        if images.len() > usize::try_from(self.cfg.rag.max_images_per_message).unwrap_or(usize::MAX)
        {
            return Err(DomainError::TooManyImages {
                max: self.cfg.rag.max_images_per_message,
            });
        }
        let boundary = repo::latest_message(&conn, chat.tenant_id, chat.id, None)
            .await?
            .map(|m| (m.created_at, m.id));

        let pf = self
            .preflight_phase(&ctx, &chat, snapshot, &req.content, &images, req.web_search)
            .await?;
        let asm = self
            .assembly_phase(&ctx, &chat, &pf, &req.content, boundary)
            .await?;

        // Reserve transaction: reserve + re-check, user message, attachments, running turn.
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let d = pf.decision.clone();
        let tenant_id = chat.tenant_id;
        let user_id = ctx.subject_id();
        let content = req.content.clone();
        let attachment_ids = req.attachment_ids.clone();
        let web_search = req.web_search;
        let started_at = now();
        let res = retry_contention(|| {
            let d = d.clone();
            let content = content.clone();
            let attachment_ids = attachment_ids.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    quota::write_reserve(tx, tenant_id, user_id, &d, started_at).await?;
                    let msg = repo::message_model(
                        user_message_id,
                        tenant_id,
                        chat_id,
                        request_id,
                        "user",
                        content,
                        started_at,
                    );
                    repo::insert_message(tx, msg).await?;
                    repo::touch_chat(tx, tenant_id, chat_id, started_at).await?;
                    validate_and_link(
                        tx,
                        tenant_id,
                        chat_id,
                        user_id,
                        user_message_id,
                        &attachment_ids,
                        started_at,
                    )
                    .await?;
                    let turn = new_turn(
                        turn_id,
                        tenant_id,
                        chat_id,
                        request_id,
                        user_id,
                        web_search,
                        started_at,
                        Some(&d),
                    );
                    repo::insert_turn(tx, turn).await?;
                    Ok(())
                })
            })
        })
        .await;
        if let Err(e) = res {
            if e.is_unique_violation() {
                let conn = self.db.conn()?;
                if repo::find_turn_by_request(&conn, chat.tenant_id, chat.id, request_id)
                    .await?
                    .is_some()
                {
                    return Err(DomainError::RequestIdConflict(format!(
                        "concurrent request {request_id}"
                    )));
                }
                return Err(DomainError::TurnAlreadyRunning);
            }
            return Err(e);
        }
        Ok(self.launch(
            TurnRuntime {
                tenant_id: chat.tenant_id,
                user_id,
                chat_id: chat.id,
                turn_id,
                request_id,
                message_id: Uuid::new_v4(),
                selected_model: chat.model.clone(),
                decision: pf.decision,
                provider: asm.provider,
                request: asm.request,
                files: asm.files,
                knowledge: asm.knowledge,
                summary_trigger: asm.summary_trigger,
                started: Instant::now(),
                started_at,
            },
            asm.summary_applied,
        ))
    }

    /// Spawns the provider task and returns the live stream.
    fn launch(self: &Arc<Self>, rt: TurnRuntime, summary_applied: Option<u32>) -> StreamStart {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let started = StreamStarted {
            request_id: rt.request_id,
            message_id: rt.message_id,
            is_new_turn: true,
            thread_summary_applied: summary_applied
                .map(|token_estimate| ThreadSummaryApplied { token_estimate }),
        };
        let svc = Arc::clone(self);
        let c = cancel.clone();
        tokio::spawn(async move { super::run::run_turn(svc, rt, tx, c).await });
        StreamStart::Live {
            started,
            rx,
            cancel,
        }
    }

    /// Side-effect-free replay of a completed turn.
    async fn replay(
        &self,
        chat: &chat::Model,
        turn: &chat_turn::Model,
    ) -> DomainResult<StreamStart> {
        let conn = self.db.conn()?;
        let msg = match turn.assistant_message_id {
            Some(id) => repo::find_message(&conn, chat.tenant_id, chat.id, id).await?,
            None => None,
        };
        let msg: Option<message::Model> = msg;
        let content = msg.as_ref().map(|m| m.content.clone()).unwrap_or_default();
        let effective = msg
            .as_ref()
            .and_then(|m| m.model.clone())
            .or_else(|| turn.effective_model.clone())
            .unwrap_or_else(|| chat.model.clone());
        let downgrade = effective != chat.model;
        let mut events = vec![StreamEvent::Started(StreamStarted {
            request_id: turn.request_id,
            message_id: msg.as_ref().map_or(Uuid::nil(), |m| m.id),
            is_new_turn: false,
            thread_summary_applied: None,
        })];
        events.push(StreamEvent::Delta(Delta {
            kind: "text",
            content,
        }));
        events.push(StreamEvent::Done(Done {
            usage: UsageOut {
                input_tokens: msg.as_ref().map_or(0, |m| m.input_tokens),
                output_tokens: msg.as_ref().map_or(0, |m| m.output_tokens),
            },
            effective_model: effective,
            selected_model: chat.model.clone(),
            quota_decision: if downgrade { "downgrade" } else { "allow" },
            downgrade_from: downgrade.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        }));
        Ok(StreamStart::Replay(events))
    }

    /// Retry (`content = None`) or edit of the latest turn.
    ///
    /// # Errors
    /// Mutation validation, preflight and setup errors.
    // Linear validate -> preflight -> setup sequence of a retry/edit.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub async fn start_mutation(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> DomainResult<StreamStart> {
        let op = if new_content.is_some() {
            "edit"
        } else {
            "retry"
        };
        if let Some(c) = &new_content {
            validate_content(c)?;
        }
        let action = if new_content.is_some() {
            actions::EDIT_TURN
        } else {
            actions::RETRY_TURN
        };
        let target = self
            .mutation_preview(&ctx, action, chat_id, request_id)
            .await?;
        let chat = target.chat.clone();
        let snapshot = self.chat_snapshot(ctx.subject_id(), &chat).await?;
        let conn = self.db.conn()?;
        let orig = turns::turn_user_message(&conn, &chat, target.turn.request_id).await?;
        let orig_text = orig.as_ref().map(|m| m.content.clone()).unwrap_or_default();
        let content = new_content.unwrap_or(orig_text);
        let orig_atts: Vec<attachment::Model> = match &orig {
            Some(m) => {
                let ids =
                    repo::message_attachment_ids(&conn, chat.tenant_id, chat.id, m.id).await?;
                repo::attachments_by_ids(&conn, chat.tenant_id, chat.id, &ids).await?
            }
            None => Vec::new(),
        };
        let images: Vec<attachment::Model> = orig_atts
            .iter()
            .filter(|a| a.attachment_kind == "image")
            .cloned()
            .collect();
        if images.len() > usize::try_from(self.cfg.rag.max_images_per_message).unwrap_or(usize::MAX)
        {
            return Err(DomainError::TooManyImages {
                max: self.cfg.rag.max_images_per_message,
            });
        }
        let web_search = target.turn.web_search_enabled;
        let pf = self
            .preflight_phase(&ctx, &chat, snapshot, &content, &images, web_search)
            .await?;

        // Mutation commit.
        let new_rid = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let outbox = Arc::clone(&self.outbox);
        let actor = ctx.subject_id();
        let started_at = now();
        let link_ids: Vec<Uuid> = orig_atts.iter().map(|a| a.id).collect();
        let (c2, t2, content2) = (chat.clone(), target.turn.clone(), content.clone());
        let res = retry_contention(|| {
            let outbox = Arc::clone(&outbox);
            let (c2, t2, content2, link_ids) =
                (c2.clone(), t2.clone(), content2.clone(), link_ids.clone());
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    turns::soft_delete_target(tx, &c2, &t2, Some(new_rid)).await?;
                    let msg = repo::message_model(
                        user_message_id,
                        c2.tenant_id,
                        c2.id,
                        new_rid,
                        "user",
                        content2,
                        started_at,
                    );
                    repo::insert_message(tx, msg).await?;
                    for aid in &link_ids {
                        repo::insert_message_attachment(
                            tx,
                            c2.tenant_id,
                            c2.id,
                            user_message_id,
                            *aid,
                            started_at,
                        )
                        .await?;
                    }
                    let turn = new_turn(
                        turn_id,
                        c2.tenant_id,
                        c2.id,
                        new_rid,
                        actor,
                        t2.web_search_enabled,
                        started_at,
                        None,
                    );
                    repo::insert_turn(tx, turn).await.map_err(|e| {
                        if e.is_unique_violation() {
                            DomainError::GenerationInProgress
                        } else {
                            e
                        }
                    })?;
                    let orig_user = turns::turn_user_message(tx, &c2, t2.request_id).await?;
                    turns::invalidate_summary_if_covers(tx, &c2, orig_user.as_ref()).await?;
                    repo::touch_chat(tx, c2.tenant_id, c2.id, started_at).await?;
                    let ev = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: if op == "edit" {
                            "turn_edit"
                        } else {
                            "turn_retry"
                        }
                        .to_owned(),
                        timestamp: started_at,
                        tenant_id: c2.tenant_id,
                        actor_user_id: actor,
                        chat_id: c2.id,
                        original_request_id: Some(t2.request_id),
                        new_request_id: Some(new_rid),
                        request_id: None,
                    });
                    let mut wakes = Wakes::default();
                    wakes.push(
                        outbox
                            .audit(tx, &ev)
                            .await
                            .map_err(turns::internal_payload)?,
                    );
                    Ok(wakes)
                })
            })
        })
        .await;
        let wakes = match res {
            Ok(w) => w,
            Err(e) => {
                self.metrics
                    .inc("turn_mutation", &[("op", op), ("result", "rejected")]);
                return Err(
                    if e.is_unique_violation() || matches!(e, DomainError::Contention(_)) {
                        DomainError::GenerationInProgress
                    } else {
                        e
                    },
                );
            }
        };
        wakes.fire();
        self.metrics
            .inc("turn_mutation", &[("op", op), ("result", "ok")]);

        // Context assembly + reserve (a failure marks the new turn failed).
        let boundary = {
            let conn = self.db.conn()?;
            repo::latest_message(&conn, chat.tenant_id, chat.id, Some(new_rid))
                .await?
                .map(|m| (m.created_at, m.id))
        };
        let asm = match self
            .assembly_phase(&ctx, &chat, &pf, &content, boundary)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                let code = if matches!(e, DomainError::ContextBudgetExceeded) {
                    "context_length_exceeded"
                } else {
                    "turn_setup_failed"
                };
                self.fail_unstarted(chat.tenant_id, turn_id, code, &e.to_string())
                    .await;
                return Err(e);
            }
        };
        let d0 = pf.decision.clone();
        let tenant_id = chat.tenant_id;
        let res = retry_contention(|| {
            let d = d0.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    quota::write_reserve(tx, tenant_id, actor, &d, started_at).await?;
                    let n = repo::update_turn_where(
                        tx,
                        tenant_id,
                        turn_id,
                        Condition::all()
                            .add(chat_turn::Column::State.eq("running"))
                            .add(chat_turn::Column::ReserveTokens.is_null()),
                        preflight_columns(&d),
                    )
                    .await?;
                    if n == 0 {
                        return Err(DomainError::internal("turn is no longer running"));
                    }
                    Ok(())
                })
            })
        })
        .await;
        if let Err(e) = res {
            let code = if matches!(e, DomainError::QuotaExceeded(QuotaScope::Tokens)) {
                "quota_exceeded"
            } else {
                "turn_setup_failed"
            };
            self.fail_unstarted(chat.tenant_id, turn_id, code, &e.to_string())
                .await;
            return Err(e);
        }
        Ok(self.launch(
            TurnRuntime {
                tenant_id: chat.tenant_id,
                user_id: actor,
                chat_id: chat.id,
                turn_id,
                request_id: new_rid,
                message_id: Uuid::new_v4(),
                selected_model: chat.model.clone(),
                decision: pf.decision,
                provider: asm.provider,
                request: asm.request,
                files: asm.files,
                knowledge: asm.knowledge,
                summary_trigger: asm.summary_trigger,
                started: Instant::now(),
                started_at,
            },
            asm.summary_applied,
        ))
    }

    /// Marks an unstarted retry/edit turn failed (no reserve, no settlement, no outbox).
    async fn fail_unstarted(&self, tenant_id: Uuid, turn_id: Uuid, code: &str, detail: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let ts = now();
        if let Err(e) = repo::update_turn_where(
            &conn,
            tenant_id,
            turn_id,
            Condition::all().add(chat_turn::Column::State.eq("running")),
            vec![
                (chat_turn::Column::State, Expr::value("failed")),
                (chat_turn::Column::ErrorCode, Expr::value(code.to_owned())),
                (
                    chat_turn::Column::ErrorDetail,
                    Expr::value(detail.chars().take(1000).collect::<String>()),
                ),
                (chat_turn::Column::CompletedAt, Expr::value(ts)),
                (chat_turn::Column::UpdatedAt, Expr::value(ts)),
            ],
        )
        .await
        {
            tracing::error!(error = %e, turn_id = %turn_id, "failed to mark unstarted turn failed");
        }
    }
}

/// Validates `attachment_ids` (tenant, uploader, chat, ready) and links them to the message.
async fn validate_and_link(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    user_id: Uuid,
    message_id: Uuid,
    ids: &[Uuid],
    ts: time::OffsetDateTime,
) -> DomainResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let atts = repo::attachments_by_ids(runner, tenant_id, chat_id, ids).await?;
    for id in ids {
        let ok = atts.iter().find(|a| a.id == *id).is_some_and(|a| {
            a.tenant_id == tenant_id
                && a.uploaded_by_user_id == user_id
                && a.chat_id == chat_id
                && ready(a)
        });
        if !ok {
            return Err(DomainError::InvalidAttachment(format!(
                "attachment {id} is not available"
            )));
        }
        repo::insert_message_attachment(runner, tenant_id, chat_id, message_id, *id, ts).await?;
    }
    Ok(())
}

fn preflight_columns(
    d: &PreflightDecision,
) -> Vec<(chat_turn::Column, sea_orm::sea_query::SimpleExpr)> {
    vec![
        (
            chat_turn::Column::ReserveTokens,
            Expr::value(d.reserve_tokens),
        ),
        (
            chat_turn::Column::MaxOutputTokensApplied,
            Expr::value(i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX)),
        ),
        (
            chat_turn::Column::ReservedCreditsMicro,
            Expr::value(d.reserved_credits_micro),
        ),
        (
            chat_turn::Column::PolicyVersionApplied,
            Expr::value(i64::try_from(d.policy_version).unwrap_or(i64::MAX)),
        ),
        (
            chat_turn::Column::EffectiveModel,
            Expr::value(d.effective.id.clone()),
        ),
        (
            chat_turn::Column::MinimalGenerationFloorApplied,
            Expr::value(i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX)),
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn new_turn(
    id: Uuid,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    user_id: Uuid,
    web_search_enabled: bool,
    ts: time::OffsetDateTime,
    d: Option<&PreflightDecision>,
) -> chat_turn::Model {
    chat_turn::Model {
        id,
        tenant_id,
        chat_id,
        request_id,
        requester_type: "user".to_owned(),
        requester_user_id: Some(user_id),
        state: "running".to_owned(),
        provider_name: None,
        provider_response_id: None,
        assistant_message_id: None,
        error_code: None,
        reserve_tokens: d.map(|d| d.reserve_tokens),
        max_output_tokens_applied: d
            .map(|d| i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX)),
        reserved_credits_micro: d.map(|d| d.reserved_credits_micro),
        policy_version_applied: d.map(|d| i64::try_from(d.policy_version).unwrap_or(i64::MAX)),
        effective_model: d.map(|d| d.effective.id.clone()),
        minimal_generation_floor_applied: d
            .map(|d| i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX)),
        error_detail: None,
        deleted_at: None,
        replaced_by_request_id: None,
        started_at: ts,
        last_progress_at: Some(ts),
        web_search_enabled,
        web_search_completed_count: 0,
        code_interpreter_completed_count: 0,
        file_search_completed_count: 0,
        completed_at: None,
        updated_at: ts,
    }
}
