//! Send-message pipeline (DESIGN §3.6 "Send Message with Streaming Response"):
//! preflight → reserve transaction → provider task → CAS finalization.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot, UsageTokens, UserLimits};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::chats::{load_chat, touch_chat};
use super::finalize::{FinalizeInput, FinalizeOutcome, Terminal};
use super::quota::{self, PreflightDecision, PreflightRequest};
use super::{AppServices, now, policy};
use crate::domain::context::{self, ContextInput, ContextPlan, HistoryMessage};
use crate::domain::error::DomainError;
use crate::domain::estimate::estimate_text_tokens;
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::db::entity::{attachments, chat_turns, chat_vector_stores, message_attachments, messages, thread_summaries};
use crate::infra::llm::transport::{ResolvedProvider, resolve_provider};
use crate::infra::llm::{
    self, ContentPart, InputMessage, LlmRequest, ProviderEvent, RawCitation, ToolSpec, TranslateState,
    codes, sse::SseParser,
};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// A citation as sent to clients.
#[derive(Debug, Clone, PartialEq)]
pub struct CitationView {
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(u64, u64)>,
}

/// Per-tier quota warning in `done`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarningView {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<DateTime<Utc>>,
}

/// Payload of `done`.
#[derive(Debug, Clone, PartialEq)]
pub struct DoneView {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: &'static str,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<WarningView>>,
}

/// Public SSE events (DESIGN §3.3 "SSE Event Definitions").
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Started {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        thread_summary_token_estimate: Option<i64>,
    },
    Ping,
    Delta { kind: &'static str, content: String },
    Tool { phase: &'static str, name: String, details: Value },
    Citations(Vec<CitationView>),
    Done(DoneView),
    Error { code: String, message: String },
}

impl StreamEvent {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }
}

/// `messages:stream` body after decoding.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// Result of stream setup.
pub enum StreamStart {
    /// Idempotent replay of a completed turn.
    Replay(Vec<StreamEvent>),
    /// Live generation; events arrive on the receiver.
    Live {
        events: mpsc::Receiver<StreamEvent>,
        cancel: CancellationToken,
    },
}

/// Everything the provider task needs.
pub struct TurnRun {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub selected_model: String,
    pub decision: PreflightDecision,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    pub citation_map: HashMap<String, (Uuid, String)>,
    pub summary_applied: Option<i64>,
    pub summary_trigger: bool,
    pub started: Instant,
}

/// Facts about the chat's attachments.
#[derive(Debug, Default)]
pub struct AttachmentFacts {
    pub has_ready_documents: bool,
    pub code_file_ids: Vec<String>,
    pub citation_map: HashMap<String, (Uuid, String)>,
}

/// Loads attachment facts of a chat.
///
/// # Errors
/// Database failure.
pub async fn attachment_facts(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<AttachmentFacts, DomainError> {
    let rows = attachments::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(attachments::Column::ChatId.eq(chat_id))
                .add(attachments::Column::DeletedAt.is_null())
                .add(attachments::Column::Status.eq("ready")),
        )
        .all(runner)
        .await?;
    let mut f = AttachmentFacts::default();
    for a in rows {
        if a.attachment_kind == "document" && a.for_file_search {
            f.has_ready_documents = true;
        }
        if let Some(pid) = &a.provider_file_id {
            if a.for_code_interpreter {
                f.code_file_ids.push(pid.clone());
            }
            f.citation_map.insert(pid.clone(), (a.id, a.filename.clone()));
        }
    }
    Ok(f)
}

/// Most recent non-deleted assistant message with usage: `input + output`.
///
/// # Errors
/// Database failure.
pub async fn prior_context_tokens(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
    let rows = messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(chat_id))
                .add(messages::Column::Role.eq("assistant"))
                .add(messages::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(messages::Column::InputTokens.gt(0))
                        .add(messages::Column::OutputTokens.gt(0)),
                ),
        )
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(messages::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .all(runner)
        .await?;
    Ok(rows.first().map_or(0, |m| m.input_tokens + m.output_tokens))
}

/// Current thread summary of a chat.
///
/// # Errors
/// Database failure.
pub async fn load_summary(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<thread_summaries::Model>, DomainError> {
    Ok(thread_summaries::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(chat_id)))
        .one(runner)
        .await?)
}

/// Recent context messages (DESIGN §4 "Recent messages query"), chronological.
///
/// # Errors
/// Database failure.
pub async fn recent_messages(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    frontier: Option<(DateTime<Utc>, Uuid)>,
    exclude_request: Option<Uuid>,
    limit: u32,
) -> Result<Vec<messages::Model>, DomainError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut cond = Condition::all()
        .add(messages::Column::ChatId.eq(chat_id))
        .add(messages::Column::RequestId.is_not_null())
        .add(messages::Column::DeletedAt.is_null())
        .add(messages::Column::IsCompressed.eq(false))
        .add(messages::Column::Role.ne("system"));
    if let Some((ts, id)) = frontier {
        cond = cond.add(
            Condition::any().add(messages::Column::CreatedAt.gt(ts)).add(
                Condition::all()
                    .add(messages::Column::CreatedAt.eq(ts))
                    .add(messages::Column::Id.gt(id)),
            ),
        );
    }
    if let Some(r) = exclude_request {
        cond = cond.add(messages::Column::RequestId.ne(r));
    }
    let mut rows = messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(cond)
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(messages::Column::Id, sea_orm::Order::Desc)
        .limit(u64::from(limit))
        .all(runner)
        .await?;
    rows.reverse();
    Ok(rows)
}

/// Finds a turn by request id (including soft-deleted ones).
///
/// # Errors
/// Database failure.
pub async fn find_turn(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::RequestId.eq(request_id)),
        )
        .one(runner)
        .await?)
}

/// The running turn of a chat, if any.
///
/// # Errors
/// Database failure.
pub async fn running_turn(runner: &impl DBRunner, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat_turns::Model>, DomainError> {
    Ok(chat_turns::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(chat_turns::Column::ChatId.eq(chat_id))
                .add(chat_turns::Column::State.eq("running"))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .one(runner)
        .await?)
}

/// Builds the replay events of a completed turn (ADR-0010).
///
/// # Errors
/// Database failure or missing assistant message.
pub async fn replay_events(
    runner: &impl DBRunner,
    chat: &crate::infra::db::entity::chats::Model,
    turn: &chat_turns::Model,
) -> Result<Vec<StreamEvent>, DomainError> {
    let msg = match turn.assistant_message_id {
        Some(id) => messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(turn.tenant_id))
            .filter(Condition::all().add(messages::Column::Id.eq(id)))
            .one(runner)
            .await?,
        None => None,
    }
    .ok_or_else(|| DomainError::internal("completed turn has no assistant message"))?;
    let effective = msg
        .model
        .clone()
        .or_else(|| turn.effective_model.clone())
        .unwrap_or_else(|| chat.model.clone());
    let downgraded = effective != chat.model;
    Ok(vec![
        StreamEvent::Started {
            request_id: turn.request_id,
            message_id: msg.id,
            is_new_turn: false,
            thread_summary_token_estimate: None,
        },
        StreamEvent::Delta {
            kind: "text",
            content: msg.content.clone(),
        },
        StreamEvent::Done(DoneView {
            input_tokens: msg.input_tokens,
            output_tokens: msg.output_tokens,
            effective_model: effective,
            selected_model: chat.model.clone(),
            quota_decision: if downgraded { "downgrade" } else { "allow" },
            downgrade_from: downgraded.then(|| chat.model.clone()),
            downgrade_reason: None,
            quota_warnings: None,
        }),
    ])
}

/// Model of a chat; a model removed from the catalog is `InvalidModel`.
///
/// # Errors
/// `InvalidModel`.
pub fn chat_model<'a>(snapshot: &'a PolicySnapshot, model_id: &str) -> Result<&'a ModelCatalogEntry, DomainError> {
    snapshot.model(model_id).ok_or(DomainError::InvalidModel)
}

/// Inputs of context + request construction shared by send and retry/edit.
pub struct BuildInput<'a> {
    pub ctx_tenant: Uuid,
    pub ctx_user: Uuid,
    pub chat_id: Uuid,
    pub decision: &'a PreflightDecision,
    pub content: &'a str,
    pub image_file_ids: Vec<String>,
    pub facts: &'a AttachmentFacts,
    pub exclude_request: Option<Uuid>,
}

/// Result of context assembly + provider resolution.
pub struct Built {
    pub plan: ContextPlan,
    pub provider: ResolvedProvider,
    pub request: LlmRequest,
    pub summary_applied: Option<i64>,
    pub has_summary: bool,
    pub citation_map: HashMap<String, (Uuid, String)>,
}

impl AppServices {
    /// Assembles the context and the provider request (before the reserve).
    ///
    /// # Errors
    /// `ContextBudgetExceeded`, provider resolution failure (`Internal`).
    pub async fn build_request(&self, runner: &impl DBRunner, b: BuildInput<'_>) -> Result<Built, DomainError> {
        let model = &b.decision.effective;
        let mut tools = Vec::new();
        let mut file_search = false;
        if b.decision.tools.file_search {
            let vs = chat_vector_stores::Entity::find()
                .secure()
                .scope_with(&AccessScope::for_tenant(b.ctx_tenant))
                .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(b.chat_id)))
                .one(runner)
                .await?
                .and_then(|r| r.vector_store_id);
            if let Some(vs) = vs {
                file_search = true;
                tools.push(ToolSpec::FileSearch {
                    vector_store_ids: vec![vs],
                    max_num_results: model.max_num_results,
                });
            }
        }
        if b.decision.tools.web_search {
            tools.push(ToolSpec::WebSearch {
                search_context_size: model.web_search_context_size.clone(),
            });
        }
        if b.decision.tools.code_interpreter && !b.facts.code_file_ids.is_empty() {
            tools.push(ToolSpec::CodeInterpreter {
                file_ids: b.facts.code_file_ids.clone(),
            });
        }
        let mut instructions = model.system_prompt.clone();
        let mut append = |s: &str| {
            if s.is_empty() {
                return;
            }
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(s);
        };
        if b.decision.tools.file_search {
            append(&self.cfg.context.file_search_guard);
        }
        if b.decision.tools.web_search {
            append(&self.cfg.context.web_search_guard);
        }
        let budgets = &model.estimation_budgets;
        let mut surcharges = 0;
        if b.decision.tools.file_search {
            surcharges += i64::from(budgets.tool_surcharge_tokens);
        }
        if b.decision.tools.web_search {
            surcharges += i64::from(budgets.web_search_surcharge_tokens);
        }
        if b.decision.tools.code_interpreter {
            surcharges += i64::from(budgets.code_interpreter_surcharge_tokens);
        }
        let summary = load_summary(runner, b.ctx_tenant, b.chat_id).await?;
        let frontier = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let history = recent_messages(
            runner,
            b.ctx_tenant,
            b.chat_id,
            frontier,
            b.exclude_request,
            self.cfg.context.recent_messages_limit,
        )
        .await?
        .into_iter()
        .map(|m| HistoryMessage {
            role: m.role,
            content: m.content,
        })
        .collect();
        let plan = context::assemble(ContextInput {
            instructions,
            summary: summary.as_ref().map(|s| s.summary_text.as_str()),
            history,
            user_message: b.content,
            image_count: b.image_file_ids.len(),
            budgets,
            context_window: model.context_window,
            max_input_tokens: model.max_input_tokens,
            max_output_tokens_applied: b.decision.max_output_tokens_applied,
            surcharges,
        })?;
        let provider = resolve_provider(&self.cfg, &model.provider_id, b.ctx_tenant).ok_or_else(|| {
            DomainError::internal(format!("provider '{}' is not configured", model.provider_id))
        })?;
        let mut input = Vec::new();
        if let Some(s) = &plan.summary_message {
            input.push(InputMessage::text("user", s.clone()));
        }
        for m in &plan.messages {
            input.push(InputMessage::text(&m.role, m.content.clone()));
        }
        let mut parts = vec![ContentPart::Text(b.content.to_owned())];
        parts.extend(b.image_file_ids.into_iter().map(ContentPart::Image));
        input.push(InputMessage {
            role: "user".to_owned(),
            parts,
        });
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), json!(b.ctx_tenant.to_string()));
        metadata.insert("user_id".into(), json!(b.ctx_user.to_string()));
        metadata.insert("chat_id".into(), json!(b.chat_id.to_string()));
        metadata.insert("request_type".into(), json!("chat"));
        metadata.insert("feature".into(), json!(llm::feature_label(&tools)));
        let request = LlmRequest {
            provider_model_id: if model.provider_model_id.is_empty() {
                model.id.clone()
            } else {
                model.provider_model_id.clone()
            },
            instructions: plan.instructions.clone(),
            input,
            max_output_tokens: b.decision.max_output_tokens_applied,
            tools,
            max_tool_calls: model.max_tool_calls,
            user: llm::provider_user(b.ctx_tenant, b.ctx_user),
            metadata,
            api_params: model.general_config.api_params.clone(),
            stream: true,
        };
        let summary_applied = plan.summary_message.as_ref().map(|_| {
            summary
                .as_ref()
                .map_or(plan.summary_tokens, |s| i64::from(s.token_estimate).max(plan.summary_tokens))
        });
        Ok(Built {
            has_summary: summary.is_some(),
            summary_applied,
            citation_map: if file_search {
                b.facts.citation_map.clone()
            } else {
                HashMap::new()
            },
            plan,
            provider,
            request,
        })
    }

    /// Validates request attachments and returns `(models, image provider ids)`.
    ///
    /// # Errors
    /// `InvalidAttachment`.
    pub async fn load_request_attachments(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<attachments::Model>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let unique: HashSet<&Uuid> = ids.iter().collect();
        if unique.len() != ids.len() {
            return Err(DomainError::InvalidAttachment("duplicate attachment id".into()));
        }
        let max = (self.cfg.rag.max_documents_per_chat + self.cfg.rag.max_images_per_message) as usize;
        if ids.len() > max {
            return Err(DomainError::InvalidAttachment("too many attachment ids".into()));
        }
        let rows = attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .filter(
                Condition::all()
                    .add(attachments::Column::Id.is_in(ids.to_vec()))
                    .add(attachments::Column::ChatId.eq(chat_id))
                    .add(attachments::Column::DeletedAt.is_null()),
            )
            .all(runner)
            .await?;
        if rows.len() != ids.len() {
            return Err(DomainError::InvalidAttachment("unknown attachment".into()));
        }
        for a in &rows {
            if a.uploaded_by_user_id != user_id || a.tenant_id != tenant_id {
                return Err(DomainError::InvalidAttachment("foreign attachment".into()));
            }
            if a.status != "ready" {
                return Err(DomainError::InvalidAttachment("attachment is not ready".into()));
            }
        }
        let order: HashMap<Uuid, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut rows = rows;
        rows.sort_by_key(|a| order.get(&a.id).copied().unwrap_or(usize::MAX));
        Ok(rows)
    }

    /// `POST /v1/chats/{id}/messages:stream` setup.
    ///
    /// # Errors
    /// Every pre-stream rejection of DESIGN §3.3.
    pub async fn send_message(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        req: SendRequest,
    ) -> Result<StreamStart, DomainError> {
        if req.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        let scope = self.authz.chat_scope(ctx, "send_message", Some(chat_id)).await?;
        let tenant_id = ctx.subject_tenant_id();
        let user_id = ctx.subject_id();
        let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
        let conn = self.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;

        // 1. idempotency, 2. parallel-turn guard
        if let Some(turn) = find_turn(&conn, chat.tenant_id, chat_id, request_id).await? {
            if turn.state == "completed" && turn.deleted_at.is_none() {
                return Ok(StreamStart::Replay(replay_events(&conn, &chat, &turn).await?));
            }
            return Err(DomainError::RequestIdConflict);
        }
        if running_turn(&conn, chat.tenant_id, chat_id).await?.is_some() {
            return Err(DomainError::TurnAlreadyRunning);
        }

        let snapshot = policy::current_snapshot(self.policy.as_ref(), user_id).await?;
        chat_model(&snapshot, &chat.model)?;
        let limits = policy::user_limits(self.policy.as_ref(), user_id, snapshot.policy_version).await?;
        if req.web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled("web_search"));
        }
        let facts = attachment_facts(&conn, chat.tenant_id, chat_id).await?;
        let referenced = self
            .load_request_attachments(&conn, chat.tenant_id, user_id, chat_id, &req.attachment_ids)
            .await?;
        let images: Vec<&attachments::Model> = referenced.iter().filter(|a| a.attachment_kind == "image").collect();
        if !images.is_empty() && snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled("images"));
        }
        if images.len() > self.cfg.rag.max_images_per_message as usize {
            return Err(DomainError::TooManyImages);
        }
        let image_ids: Vec<String> = images.iter().filter_map(|a| a.provider_file_id.clone()).collect();
        let prior = prior_context_tokens(&conn, chat.tenant_id, chat_id).await?;
        let decision = self
            .preflight(&conn, &snapshot, &limits, &chat.model, tenant_id, user_id, PreflightFacts {
                content: &req.content,
                image_count: images.len(),
                prior,
                facts: &facts,
                web_search: req.web_search,
            })
            .await?;
        let built = self
            .build_request(&conn, BuildInput {
                ctx_tenant: chat.tenant_id,
                ctx_user: user_id,
                chat_id,
                decision: &decision,
                content: &req.content,
                image_file_ids: image_ids,
                facts: &facts,
                exclude_request: None,
            })
            .await?;
        drop(conn);

        // Reserve transaction.
        let turn_id = Uuid::new_v4();
        let user_msg_id = Uuid::new_v4();
        let attachment_ids = req.attachment_ids.clone();
        let content = req.content.clone();
        let web_search = req.web_search;
        let d = decision.clone();
        let svc = Arc::clone(self);
        let chat_tenant = chat.tenant_id;
        let tx_result = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    quota::reserve(tx, chat_tenant, user_id, &d).await?;
                    let ts = now();
                    insert_message(tx, NewMessage {
                        id: user_msg_id,
                        tenant_id: chat_tenant,
                        chat_id,
                        request_id,
                        role: "user",
                        content: content.clone(),
                        model: None,
                        usage: None,
                        provider_response_id: None,
                        created_at: ts,
                    })
                    .await?;
                    touch_chat(tx, chat_tenant, chat_id).await?;
                    svc.load_request_attachments(tx, chat_tenant, user_id, chat_id, &attachment_ids)
                        .await?;
                    link_attachments(tx, chat_tenant, chat_id, user_msg_id, &attachment_ids).await?;
                    insert_turn(tx, NewTurn {
                        id: turn_id,
                        tenant_id: chat_tenant,
                        chat_id,
                        request_id,
                        user_id,
                        web_search_enabled: web_search,
                        preflight: Some(&d),
                    })
                    .await?;
                    Ok::<_, DomainError>(())
                })
            })
            .await;
        if let Err(e) = tx_result {
            return Err(self.map_insert_conflict(chat_tenant, chat_id, request_id, e).await);
        }

        let run = TurnRun {
            tenant_id: chat_tenant,
            user_id,
            chat_id,
            turn_id,
            request_id,
            message_id: Uuid::new_v4(),
            selected_model: chat.model.clone(),
            decision,
            provider: built.provider,
            request: built.request,
            citation_map: built.citation_map,
            summary_applied: built.summary_applied,
            summary_trigger: self.cfg.thread_summary_worker.enabled
                && context::summary_trigger(
                    &built.plan,
                    built.has_summary,
                    self.cfg.thread_summary_worker.compression_threshold_pct,
                ),
            started: Instant::now(),
        };
        Ok(self.spawn_turn(run))
    }

    /// Maps a failed reserve transaction (unique-index races, ADR-0004).
    pub async fn map_insert_conflict(&self, tenant_id: Uuid, chat_id: Uuid, request_id: Uuid, e: DomainError) -> DomainError {
        if !matches!(e, DomainError::UniqueViolation) {
            return e;
        }
        if let Ok(conn) = self.conn()
            && let Ok(Some(_)) = find_turn(&conn, tenant_id, chat_id, request_id).await
        {
            return DomainError::RequestIdConflict;
        }
        DomainError::TurnAlreadyRunning
    }

    /// Runs the quota preflight (cascade, tool quotas) and the input/vision guards.
    ///
    /// # Errors
    /// `QuotaExceeded`, `InputTooLong`, `VisionNotSupported`.
    #[allow(clippy::too_many_arguments)]
    pub async fn preflight(
        &self,
        runner: &impl DBRunner,
        snapshot: &PolicySnapshot,
        limits: &UserLimits,
        selected_model: &str,
        tenant_id: Uuid,
        user_id: Uuid,
        f: PreflightFacts<'_>,
    ) -> Result<PreflightDecision, DomainError> {
        let periods = quota::current_periods(now());
        let usage = quota::read_usage(runner, tenant_id, user_id, &periods).await?;
        let req = PreflightRequest {
            selected_model: selected_model.to_owned(),
            message_bytes: f.content.len(),
            image_count: f.image_count,
            prior_context_tokens: f.prior,
            has_ready_documents: f.facts.has_ready_documents,
            has_ready_code_files: !f.facts.code_file_ids.is_empty(),
            web_search_requested: f.web_search,
        };
        let decision = quota::evaluate_cascade(
            snapshot,
            limits,
            &usage,
            &req,
            self.cfg.streaming.max_output_tokens,
            self.cfg.estimation_budgets.minimal_generation_floor,
            self.cfg.quota.web_search_daily_quota,
            self.cfg.quota.code_interpreter_daily_quota,
            periods,
        )?;
        let model = &decision.effective;
        if model.max_input_tokens > 0
            && estimate_text_tokens(f.content.len(), &model.estimation_budgets) > i64::from(model.max_input_tokens)
        {
            return Err(DomainError::InputTooLong);
        }
        if f.image_count > 0 && !model.supports_vision() {
            return Err(DomainError::VisionNotSupported);
        }
        Ok(decision)
    }

    /// Spawns the provider task and returns the event receiver.
    pub fn spawn_turn(self: &Arc<Self>, run: TurnRun) -> StreamStart {
        let cap = usize::from(self.cfg.streaming.sse_channel_capacity);
        let (tx, rx) = mpsc::channel(cap);
        let cancel = CancellationToken::new();
        let svc = Arc::clone(self);
        let c = cancel.clone();
        tokio::spawn(async move {
            svc.run_turn(run, tx, c).await;
        });
        StreamStart::Live { events: rx, cancel }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_turn(self: Arc<Self>, run: TurnRun, tx: mpsc::Sender<StreamEvent>, cancel: CancellationToken) {
        let started = StreamEvent::Started {
            request_id: run.request_id,
            message_id: run.message_id,
            is_new_turn: true,
            thread_summary_token_estimate: run.summary_applied,
        };
        let mut st = RunState::default();
        if tx.send(started).await.is_err() {
            self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
            return;
        }
        let body = llm::build_body(run.provider.kind, &run.request);
        let http_req = http::Request::builder()
            .method("POST")
            .uri(run.provider.chat_uri(&run.request.provider_model_id))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(oagw_sdk::Body::Bytes(bytes::Bytes::from(
                serde_json::to_vec(&body).unwrap_or_default(),
            )));
        let Ok(http_req) = http_req else {
            let t = Terminal::failed(codes::PROVIDER_ERROR, "invalid provider request".into(), None);
            self.finish(&run, &mut st, t, &tx).await;
            return;
        };
        let ping_every = Duration::from_secs(u64::from(self.cfg.streaming.sse_ping_interval_seconds));
        let send = self.transport.send(http_req);
        tokio::pin!(send);
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + ping_every, ping_every);
        let resp = loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
                    return;
                }
                () = tx.closed() => {
                    self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
                    return;
                }
                _ = ping.tick() => {
                    if tx.send(StreamEvent::Ping).await.is_err() {
                        self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
                        return;
                    }
                }
                r = &mut send => break r,
            }
        };
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let t = gateway_terminal(&e);
                self.finish(&run, &mut st, t, &tx).await;
                return;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let gateway = resp.extensions().get::<oagw_sdk::api::ErrorSource>().copied()
                == Some(oagw_sdk::api::ErrorSource::Gateway);
            let body = resp.into_body().into_bytes().await.unwrap_or_default();
            let t = status_terminal(status.as_u16(), gateway, retry_after, &body);
            self.finish(&run, &mut st, t, &tx).await;
            return;
        }
        let mut stream = resp.into_body().into_stream();
        let mut parser = SseParser::new();
        let mut ts = TranslateState::default();
        let mut last_progress = Instant::now();
        loop {
            let chunk = tokio::select! {
                () = cancel.cancelled() => {
                    self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
                    return;
                }
                () = tx.closed() => {
                    self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
                    return;
                }
                _ = ping.tick(), if !st.content_started => {
                    if tx.send(StreamEvent::Ping).await.is_err() {
                        self.finish(&run, &mut st, Terminal::Cancelled, &tx).await;
                        return;
                    }
                    continue;
                }
                c = stream.next() => c,
            };
            let frames = match chunk {
                Some(Ok(bytes)) => parser.feed(&bytes),
                Some(Err(e)) => {
                    let msg = format!("provider stream failed: {e}");
                    self.finish(&run, &mut st, Terminal::failed(codes::PROVIDER_ERROR, msg, None), &tx)
                        .await;
                    return;
                }
                None => {
                    let mut tail = Vec::new();
                    if let Some(f) = parser.finish() {
                        tail.push(f);
                    }
                    if !tail.is_empty() {
                        for f in tail {
                            for ev in llm::translate(run.provider.kind, &mut ts, f.event.as_deref(), &f.data) {
                                if let Some(t) = self.on_event(&run, &mut st, ev, &tx, &cancel).await {
                                    self.finish(&run, &mut st, t, &tx).await;
                                    return;
                                }
                            }
                        }
                    }
                    let t = Terminal::failed(
                        codes::PROVIDER_ERROR,
                        "Provider stream ended unexpectedly".into(),
                        None,
                    );
                    self.finish(&run, &mut st, t, &tx).await;
                    return;
                }
            };
            for f in frames {
                for ev in llm::translate(run.provider.kind, &mut ts, f.event.as_deref(), &f.data) {
                    if let Some(t) = self.on_event(&run, &mut st, ev, &tx, &cancel).await {
                        self.finish(&run, &mut st, t, &tx).await;
                        return;
                    }
                }
            }
            if last_progress.elapsed() >= PROGRESS_INTERVAL && st.content_started {
                last_progress = Instant::now();
                self.touch_progress(&run, &st).await;
            }
        }
    }

    /// Handles one provider event; returns a terminal outcome when the turn ends.
    async fn on_event(
        &self,
        run: &TurnRun,
        st: &mut RunState,
        ev: ProviderEvent,
        tx: &mpsc::Sender<StreamEvent>,
        _cancel: &CancellationToken,
    ) -> Option<Terminal> {
        let out = match ev {
            ProviderEvent::TextDelta(t) => {
                if st.first_token.is_none() {
                    st.first_token = Some(run.started.elapsed());
                }
                st.content_started = true;
                st.text.push_str(&t);
                Some(StreamEvent::Delta { kind: "text", content: t })
            }
            ProviderEvent::ReasoningDelta(t) => {
                st.content_started = true;
                Some(StreamEvent::Delta {
                    kind: "reasoning",
                    content: t,
                })
            }
            ProviderEvent::ToolStart { name, details } => {
                st.content_started = true;
                match name.as_str() {
                    "web_search" => {
                        st.web_started += 1;
                        if st.web_started > self.cfg.quota.web_search_max_calls_per_message {
                            return Some(Terminal::failed(
                                codes::WEB_SEARCH_CALLS_EXCEEDED,
                                "The model exceeded the web search call limit for this message".into(),
                                None,
                            ));
                        }
                    }
                    "code_interpreter" => {
                        st.ci_started += 1;
                        if st.ci_started > self.cfg.quota.code_interpreter_max_calls_per_message {
                            return Some(Terminal::failed(
                                codes::CODE_INTERPRETER_CALLS_EXCEEDED,
                                "The model exceeded the code interpreter call limit for this message".into(),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
                Some(StreamEvent::Tool {
                    phase: "start",
                    name,
                    details,
                })
            }
            ProviderEvent::ToolDone { name, details } => {
                match name.as_str() {
                    "web_search" => st.web_done += 1,
                    "code_interpreter" => st.ci_done += 1,
                    "file_search" => st.fs_done += 1,
                    _ => {}
                }
                self.touch_progress(run, st).await;
                Some(StreamEvent::Tool {
                    phase: "done",
                    name,
                    details,
                })
            }
            ProviderEvent::Citation(c) => {
                if let Some(v) = map_citation(&c, &run.citation_map)
                    && !st.citations.contains(&v)
                {
                    st.citations.push(v);
                }
                None
            }
            ProviderEvent::Completed {
                usage,
                response_id,
                incomplete_reason,
            } => {
                if let Some(r) = &incomplete_reason {
                    tracing::warn!(reason = %r, request_id = %run.request_id, "stream incomplete");
                }
                if !st.citations.is_empty() {
                    let items = std::mem::take(&mut st.citations);
                    if tx.send(StreamEvent::Citations(items)).await.is_err() {
                        return Some(Terminal::Cancelled);
                    }
                }
                return Some(Terminal::Completed { usage, response_id });
            }
            ProviderEvent::Failed { code, message, usage } => {
                return Some(Terminal::failed(code, sanitize_provider_message(&message), usage));
            }
        };
        if let Some(e) = out
            && tx.send(e).await.is_err()
        {
            return Some(Terminal::Cancelled);
        }
        None
    }

    async fn touch_progress(&self, run: &TurnRun, st: &RunState) {
        let Ok(conn) = self.conn() else { return };
        let _ = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::LastProgressAt, Expr::value(Some(now())))
            .col_expr(chat_turns::Column::WebSearchCompletedCount, Expr::value(st.web_done as i32))
            .col_expr(chat_turns::Column::CodeInterpreterCompletedCount, Expr::value(st.ci_done as i32))
            .col_expr(chat_turns::Column::FileSearchCompletedCount, Expr::value(st.fs_done as i32))
            .filter(
                Condition::all()
                    .add(chat_turns::Column::Id.eq(run.turn_id))
                    .add(chat_turns::Column::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(run.tenant_id))
            .exec(&conn)
            .await;
    }

    /// Finalizes the turn and emits the gated terminal event.
    async fn finish(&self, run: &TurnRun, st: &mut RunState, terminal: Terminal, tx: &mpsc::Sender<StreamEvent>) {
        let is_cancel = matches!(terminal, Terminal::Cancelled);
        let failed_code = match &terminal {
            Terminal::Failed { code, .. } => Some((code.clone(), terminal.message())),
            _ => None,
        };
        let completed_usage = match &terminal {
            Terminal::Completed { usage, .. } => Some(usage.unwrap_or_default()),
            _ => None,
        };
        let input = FinalizeInput {
            tenant_id: run.tenant_id,
            user_id: run.user_id,
            chat_id: run.chat_id,
            turn_id: run.turn_id,
            request_id: run.request_id,
            message_id: run.message_id,
            selected_model: run.selected_model.clone(),
            decision: run.decision.clone(),
            terminal,
            text: st.text.clone(),
            web_search_calls: st.web_done,
            code_interpreter_calls: st.ci_done,
            file_search_calls: st.fs_done,
            summary_trigger: run.summary_trigger,
            ttft_ms: st.first_token.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            total_ms: u64::try_from(run.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let outcome = self.finalize_turn(input).await;
        if is_cancel {
            return;
        }
        let ev = match outcome {
            FinalizeOutcome::Completed => {
                let u = completed_usage.unwrap_or_default();
                let warnings = self
                    .quota_warnings(run.tenant_id, run.user_id, &run.decision.limits)
                    .await
                    .ok()
                    .map(|ws| {
                        ws.into_iter()
                            .map(|w| WarningView {
                                tier: w.tier,
                                period: w.period.as_str(),
                                remaining_percentage: w.remaining_percentage,
                                warning: w.warning,
                                exhausted: w.exhausted,
                                next_reset: (w.warning || w.exhausted).then_some(w.next_reset),
                            })
                            .collect()
                    });
                let downgrade = run.decision.quota_decision() == "downgrade";
                StreamEvent::Done(DoneView {
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    effective_model: run.decision.effective.id.clone(),
                    selected_model: run.selected_model.clone(),
                    quota_decision: run.decision.quota_decision(),
                    downgrade_from: downgrade.then(|| run.selected_model.clone()),
                    downgrade_reason: if downgrade {
                        run.decision.downgrade_reason.map(str::to_owned)
                    } else {
                        None
                    },
                    quota_warnings: warnings,
                })
            }
            FinalizeOutcome::Failed => {
                let (code, message) = failed_code.unwrap_or_else(|| {
                    (codes::PROVIDER_ERROR.to_owned(), "Provider error".to_owned())
                });
                StreamEvent::Error { code, message }
            }
            FinalizeOutcome::MessagePersistenceFailed => StreamEvent::Error {
                code: codes::MESSAGE_PERSISTENCE_FAILED.to_owned(),
                message: "The assistant message could not be saved".to_owned(),
            },
            FinalizeOutcome::FinalizationFailed => match failed_code {
                Some((code, message)) => StreamEvent::Error { code, message },
                None => StreamEvent::Error {
                    code: codes::FINALIZATION_FAILED.to_owned(),
                    message: "The response could not be finalized".to_owned(),
                },
            },
            FinalizeOutcome::Lost => return,
        };
        let _ = tx.send(ev).await;
    }
}

/// Facts used by [`AppServices::preflight`].
pub struct PreflightFacts<'a> {
    pub content: &'a str,
    pub image_count: usize,
    pub prior: i64,
    pub facts: &'a AttachmentFacts,
    pub web_search: bool,
}

#[derive(Default)]
struct RunState {
    text: String,
    content_started: bool,
    first_token: Option<Duration>,
    web_started: u32,
    ci_started: u32,
    web_done: u32,
    ci_done: u32,
    fs_done: u32,
    citations: Vec<CitationView>,
}

fn map_citation(c: &RawCitation, map: &HashMap<String, (Uuid, String)>) -> Option<CitationView> {
    match c {
        RawCitation::Web {
            url,
            title,
            snippet,
            span,
        } => Some(CitationView {
            source: "web",
            title: title.clone(),
            url: Some(url.clone()),
            attachment_id: None,
            snippet: snippet.clone(),
            span: *span,
        }),
        RawCitation::File { file_id, span, .. } => map.get(file_id).map(|(id, name)| CitationView {
            source: "file",
            title: name.clone(),
            url: None,
            attachment_id: Some(*id),
            snippet: String::new(),
            span: *span,
        }),
    }
}

fn gateway_terminal(e: &toolkit_canonical_errors::CanonicalError) -> Terminal {
    use toolkit_canonical_errors::CanonicalError as C;
    match e {
        C::DeadlineExceeded { .. } => Terminal::failed(codes::PROVIDER_TIMEOUT, "Provider request timed out".into(), None),
        C::ResourceExhausted { .. } => Terminal::failed(codes::RATE_LIMITED, "Provider rate limit reached".into(), None),
        _ => Terminal::failed(
            codes::PROVIDER_ERROR,
            sanitize_provider_message(&format!("Provider is currently unavailable: {}", e.detail())),
            None,
        ),
    }
}

/// Maps a non-2xx provider response to a terminal outcome.
#[must_use]
pub fn status_terminal(status: u16, gateway: bool, retry_after: Option<u64>, body: &[u8]) -> Terminal {
    let message = sanitize_provider_message(&llm::error_message_from_body(body));
    match status {
        429 => {
            let m = match retry_after {
                Some(s) => format!("Provider rate limit reached; retry after {s} seconds"),
                None => "Provider rate limit reached".to_owned(),
            };
            Terminal::failed(codes::RATE_LIMITED, m, None)
        }
        504 if gateway => Terminal::failed(codes::PROVIDER_TIMEOUT, "Provider request timed out".into(), None),
        _ => Terminal::failed(codes::PROVIDER_ERROR, message, None),
    }
}

/// New message row.
pub struct NewMessage {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub role: &'static str,
    pub content: String,
    pub model: Option<String>,
    pub usage: Option<UsageTokens>,
    pub provider_response_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Inserts a message.
///
/// # Errors
/// Database failure.
pub async fn insert_message(runner: &impl DBRunner, m: NewMessage) -> Result<(), DomainError> {
    let u = m.usage.unwrap_or_default();
    let am = messages::ActiveModel {
        id: sea_orm::Set(m.id),
        tenant_id: sea_orm::Set(m.tenant_id),
        chat_id: sea_orm::Set(m.chat_id),
        request_id: sea_orm::Set(Some(m.request_id)),
        role: sea_orm::Set(m.role.to_owned()),
        content: sea_orm::Set(m.content),
        content_type: sea_orm::Set("text".to_owned()),
        token_estimate: sea_orm::Set(0),
        provider_response_id: sea_orm::Set(m.provider_response_id),
        request_kind: sea_orm::Set("chat".to_owned()),
        features_used: sea_orm::Set(json!([])),
        input_tokens: sea_orm::Set(u.input_tokens),
        output_tokens: sea_orm::Set(u.output_tokens),
        cache_read_input_tokens: sea_orm::Set(u.cache_read_input_tokens),
        cache_write_input_tokens: sea_orm::Set(u.cache_write_input_tokens),
        reasoning_tokens: sea_orm::Set(u.reasoning_tokens),
        model: sea_orm::Set(m.model),
        is_compressed: sea_orm::Set(false),
        created_at: sea_orm::Set(m.created_at),
        deleted_at: sea_orm::Set(None),
    };
    messages::Entity::insert(am)
        .secure()
        .scope_unchecked(&AccessScope::for_tenant(m.tenant_id))?
        .exec(runner)
        .await?;
    Ok(())
}

/// Inserts `message_attachments` rows.
///
/// # Errors
/// Database failure.
pub async fn link_attachments(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
    message_id: Uuid,
    attachment_ids: &[Uuid],
) -> Result<(), DomainError> {
    for a in attachment_ids {
        let am = message_attachments::ActiveModel {
            tenant_id: sea_orm::Set(tenant_id),
            chat_id: sea_orm::Set(chat_id),
            message_id: sea_orm::Set(message_id),
            attachment_id: sea_orm::Set(*a),
            created_at: sea_orm::Set(now()),
        };
        message_attachments::Entity::insert(am)
            .secure()
            .scope_unchecked(&AccessScope::for_tenant(tenant_id))?
            .exec(runner)
            .await?;
    }
    Ok(())
}

/// New turn row.
pub struct NewTurn<'a> {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub user_id: Uuid,
    pub web_search_enabled: bool,
    pub preflight: Option<&'a PreflightDecision>,
}

/// Inserts a `running` turn (preflight fields NULL for retry/edit).
///
/// # Errors
/// Database failure / unique violation.
pub async fn insert_turn(runner: &impl DBRunner, t: NewTurn<'_>) -> Result<(), DomainError> {
    let ts = now();
    let p = t.preflight;
    let am = chat_turns::ActiveModel {
        id: sea_orm::Set(t.id),
        tenant_id: sea_orm::Set(t.tenant_id),
        chat_id: sea_orm::Set(t.chat_id),
        request_id: sea_orm::Set(t.request_id),
        requester_type: sea_orm::Set("user".to_owned()),
        requester_user_id: sea_orm::Set(Some(t.user_id)),
        state: sea_orm::Set("running".to_owned()),
        provider_name: sea_orm::Set(None),
        provider_response_id: sea_orm::Set(None),
        assistant_message_id: sea_orm::Set(None),
        error_code: sea_orm::Set(None),
        reserve_tokens: sea_orm::Set(p.map(|d| d.reserve_tokens)),
        max_output_tokens_applied: sea_orm::Set(p.map(|d| i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX))),
        reserved_credits_micro: sea_orm::Set(p.map(|d| d.reserved_credits_micro)),
        policy_version_applied: sea_orm::Set(p.map(|d| i64::try_from(d.policy_version).unwrap_or(i64::MAX))),
        effective_model: sea_orm::Set(p.map(|d| d.effective.id.clone())),
        minimal_generation_floor_applied: sea_orm::Set(
            p.map(|d| i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX)),
        ),
        error_detail: sea_orm::Set(None),
        deleted_at: sea_orm::Set(None),
        replaced_by_request_id: sea_orm::Set(None),
        started_at: sea_orm::Set(ts),
        last_progress_at: sea_orm::Set(Some(ts)),
        web_search_enabled: sea_orm::Set(t.web_search_enabled),
        web_search_completed_count: sea_orm::Set(0),
        code_interpreter_completed_count: sea_orm::Set(0),
        file_search_completed_count: sea_orm::Set(0),
        completed_at: sea_orm::Set(None),
        updated_at: sea_orm::Set(ts),
    };
    chat_turns::Entity::insert(am)
        .secure()
        .scope_unchecked(&AccessScope::for_tenant(t.tenant_id))?
        .exec(runner)
        .await?;
    Ok(())
}
