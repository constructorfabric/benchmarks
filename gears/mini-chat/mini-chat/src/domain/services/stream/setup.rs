//! Stream setup: every step before the provider call (DESIGN section 3.6,
//! "Send Message with Streaming Response"). Building blocks shared by the
//! send path and the retry/edit paths.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use super::events::ThreadSummaryInfo;
use crate::config::{MiniChatConfig, RagConfig};
use crate::domain::context::{ContextInput, HistoryItem, assemble_context, summary_trigger};
use crate::domain::enums::{AttachmentKind, AttachmentStatus, MessageRole};
use crate::domain::error::DomainError;
use crate::domain::estimation::{
    ToolContext, ToolGates, effective_input_budget, estimate_text_tokens, token_budget,
    tool_surcharges,
};
use crate::domain::ports::{LlmRequest, RequestMetadata, ToolSpec, feature_label, provider_user};
use crate::domain::services::quota_service::{PreflightDecision, QuotaService, ReserveRequest};
use crate::domain::tools::{select_tools, tool_guards};
use crate::infra::db::entities::{attachment, chat};
use crate::infra::db::repos::message_repo::{self, NewMessage};
use crate::infra::db::repos::turn_repo::{self, NewRunningTurn, TurnPreflight};
use crate::infra::db::repos::{attachment_repo, chat_repo};
use crate::infra::llm::knowledge::{KnowledgeTarget, search_knowledge_tool};

/// `request_kind` of chat messages.
const REQUEST_KIND_CHAT: &str = "chat";

/// `content` must not be empty or whitespace-only.
pub(super) fn validate_content(content: &str) -> Result<(), DomainError> {
    if content.trim().is_empty() {
        return Err(DomainError::EmptyContent);
    }
    Ok(())
}

/// At most `max_documents_per_chat + max_images_per_message` ids, all unique.
pub(super) fn validate_attachment_ids(ids: &[Uuid], rag: &RagConfig) -> Result<(), DomainError> {
    let max = u64::from(rag.max_documents_per_chat) + u64::from(rag.max_images_per_message);
    let unique: HashSet<&Uuid> = ids.iter().collect();
    if u64::try_from(ids.len()).unwrap_or(u64::MAX) > max || unique.len() != ids.len() {
        return Err(DomainError::InvalidAttachment);
    }
    Ok(())
}

/// What the preflight and the context assembly read from the chat.
pub(super) struct TurnFacts {
    /// `(created_at, id)` of the latest live message (snapshot boundary).
    pub boundary: Option<(OffsetDateTime, Uuid)>,
    /// `input + output` tokens of the latest assistant message with usage.
    pub prior_context_tokens: i64,
    /// The chat's ready, live attachments.
    pub ready: Vec<attachment::Model>,
    /// The request's images: owned by the caller, ready, in the chat, in
    /// request order.
    pub images: Vec<attachment::Model>,
    /// Knowledge-search target when knowledge search is available for the
    /// request (set by the stream service; `file_search` still wins).
    pub knowledge: Option<KnowledgeTarget>,
}

impl TurnFacts {
    pub fn tool_ctx(&self, web_search_requested: bool) -> ToolContext {
        ToolContext {
            chat_has_ready_documents: self.ready.iter().any(|a| {
                a.for_file_search && a.attachment_kind == AttachmentKind::Document.as_str()
            }),
            chat_has_ready_ci_files: !self.ci_file_ids().is_empty(),
            web_search_requested,
        }
    }

    pub fn num_images(&self) -> u32 {
        u32::try_from(self.images.len()).unwrap_or(u32::MAX)
    }

    fn ci_file_ids(&self) -> Vec<String> {
        self.ready
            .iter()
            .filter(|a| a.for_code_interpreter)
            .filter_map(|a| a.provider_file_id.clone())
            .collect()
    }

    fn image_file_ids(&self) -> Vec<String> {
        self.images
            .iter()
            .filter_map(|a| a.provider_file_id.clone())
            .collect()
    }

    /// `provider_file_id -> (attachment_id, filename)` of the ready
    /// attachments (citation resolution, DESIGN section 4).
    fn citation_map(&self) -> HashMap<String, (Uuid, String)> {
        self.ready
            .iter()
            .filter_map(|a| {
                a.provider_file_id
                    .clone()
                    .map(|f| (f, (a.id, a.filename.clone())))
            })
            .collect()
    }
}

/// Reads the snapshot boundary, the prior context tokens, the chat's ready
/// attachments and the request's usable images; more images than
/// `max_images_per_message` is `TooManyImages`.
///
/// `replaced_request_id` is the turn a retry/edit replaces: its assistant
/// usage is not prior context of the replacement. (Its messages may still
/// set the boundary; they are soft-deleted before the context is read.)
pub(super) async fn gather_facts(
    runner: &impl DBRunner,
    chat: &chat::Model,
    user_id: Uuid,
    attachment_ids: &[Uuid],
    rag: &RagConfig,
    replaced_request_id: Option<Uuid>,
) -> Result<TurnFacts, DomainError> {
    let boundary = message_repo::snapshot_boundary(runner, chat.tenant_id, chat.id).await?;
    let prior_context_tokens = message_repo::latest_assistant_with_usage(
        runner,
        chat.tenant_id,
        chat.id,
        replaced_request_id,
    )
    .await?
    .map_or(0, |(i, o)| i.saturating_add(o));
    let ready = attachment_repo::ready_in_chat(runner, chat.tenant_id, chat.id).await?;
    let mut requested: HashMap<Uuid, attachment::Model> =
        attachment_repo::live_in_chat_by_ids(runner, chat.tenant_id, chat.id, attachment_ids)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
    let images: Vec<attachment::Model> = attachment_ids
        .iter()
        .filter_map(|id| requested.remove(id))
        .filter(|a| {
            a.attachment_kind == AttachmentKind::Image.as_str()
                && a.status == AttachmentStatus::Ready.as_str()
                && a.uploaded_by_user_id == user_id
        })
        .collect();
    if u64::try_from(images.len()).unwrap_or(u64::MAX) > u64::from(rag.max_images_per_message) {
        return Err(DomainError::TooManyImages);
    }
    Ok(TurnFacts {
        boundary,
        prior_context_tokens,
        ready,
        images,
        knowledge: None,
    })
}

/// Checks on the effective model after the preflight: the input token limit
/// (`max_input_tokens > 0`), the `disable_images` kill switch and vision.
pub(super) fn check_effective_model(
    decision: &PreflightDecision,
    content: &str,
    num_images: u32,
) -> Result<(), DomainError> {
    let m = &decision.effective_model;
    if m.max_input_tokens > 0
        && estimate_text_tokens(content.len(), &m.estimation_budgets)
            > i64::from(m.max_input_tokens)
    {
        return Err(DomainError::InputTooLong);
    }
    if num_images > 0 {
        if decision.snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled { subject: "images" });
        }
        if !m.supports_vision() {
            return Err(DomainError::VisionNotSupported);
        }
    }
    Ok(())
}

/// The provider request of a turn and what the stream needs about it.
pub(super) struct PreparedRequest {
    pub request: LlmRequest,
    /// Set when the thread summary was kept in the context.
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
    /// Thread summary trigger candidate for finalization.
    pub summary_trigger: bool,
    /// Citation map (empty unless `file_search` is sent).
    pub citation_map: HashMap<String, (Uuid, String)>,
    /// Knowledge-search target when `search_knowledge` is offered.
    pub knowledge: Option<KnowledgeTarget>,
}

/// Context assembly (system prompt, tool guards, thread summary, recent
/// messages up to the snapshot boundary, user message, images) and the
/// provider request.
///
/// # Errors
/// `ContextBudgetExceeded`, database failure.
pub(super) async fn prepare_request(
    runner: &impl DBRunner,
    cfg: &MiniChatConfig,
    chat: &chat::Model,
    user_id: Uuid,
    decision: &PreflightDecision,
    facts: &TurnFacts,
    content: &str,
) -> Result<PreparedRequest, DomainError> {
    let m = &decision.effective_model;
    let summary = message_repo::thread_summary(runner, chat.tenant_id, chat.id).await?;
    let frontier = summary
        .as_ref()
        .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
    let history = match facts.boundary {
        Some(boundary) => message_repo::recent_for_context(
            runner,
            chat.tenant_id,
            chat.id,
            boundary,
            frontier,
            u64::from(cfg.context.recent_messages_limit),
        )
        .await?
        .into_iter()
        .filter_map(|msg| {
            MessageRole::parse(&msg.role).map(|role| HistoryItem {
                role,
                content: msg.content,
            })
        })
        .collect(),
        None => Vec::new(),
    };

    let vector_store_id = if decision.tools.file_search {
        attachment_repo::chat_vector_store_id(runner, chat.tenant_id, chat.id).await?
    } else {
        None
    };
    let mut tools = select_tools(
        &decision.tools,
        vector_store_id.as_deref(),
        m.max_num_results,
        &m.web_search_context_size,
        &facts.ci_file_ids(),
    );
    let sent = sent_gates(&tools);
    let max_tool_calls = (!tools.is_empty()).then_some(m.max_tool_calls);
    // Mutual exclusion: file_search wins over knowledge search.
    let knowledge = facts.knowledge.clone().filter(|_| !sent.file_search);
    let mut guards = tool_guards(&tools, &cfg.context);
    if knowledge.is_some() {
        tools.push(search_knowledge_tool());
        guards.push(cfg.knowledge_search.guard.as_str());
    }
    let budget = token_budget(
        m,
        decision.max_output_tokens_applied,
        tool_surcharges(sent, &m.estimation_budgets),
    )?;
    let plan = assemble_context(ContextInput {
        system_prompt: &m.system_prompt,
        guards,
        summary: summary.as_ref().map(|s| s.summary_text.as_str()),
        history,
        user_message: content,
        image_file_ids: facts.image_file_ids(),
        budgets: &m.estimation_budgets,
        token_budget: budget,
    })?;

    let thread_summary_applied = match (&summary, plan.summary_token_estimate) {
        (Some(s), Some(_)) => Some(ThreadSummaryInfo {
            token_estimate: u32::try_from(s.token_estimate).unwrap_or(0),
        }),
        _ => None,
    };
    let trigger = summary_trigger(
        cfg.thread_summary_worker.enabled,
        summary.is_some(),
        plan.messages_truncated,
        plan.assembled_tokens,
        effective_input_budget(m, decision.max_output_tokens_applied),
        cfg.thread_summary_worker.compression_threshold_pct,
    );
    let citation_map = if sent.file_search {
        facts.citation_map()
    } else {
        HashMap::new()
    };
    let (tenant, user) = (chat.tenant_id.to_string(), user_id.to_string());
    let feature = feature_label(&tools);
    let request = LlmRequest {
        model: m.provider_model_id.clone(),
        instructions: plan.instructions,
        input: plan.input,
        tools,
        max_output_tokens: u32::try_from(decision.max_output_tokens_applied).unwrap_or(u32::MAX),
        api_params: m.general_config.api_params.clone(),
        max_tool_calls,
        user: provider_user(&tenant, &user),
        metadata: RequestMetadata {
            tenant_id: tenant,
            user_id: user,
            chat_id: chat.id.to_string(),
            request_type: "chat",
            feature,
        },
        stream: true,
    };
    Ok(PreparedRequest {
        request,
        thread_summary_applied,
        summary_trigger: trigger,
        citation_map,
        knowledge,
    })
}

/// The gates of the tools actually in the request (surcharges apply only to
/// them).
fn sent_gates(tools: &[ToolSpec]) -> ToolGates {
    ToolGates {
        file_search: tools
            .iter()
            .any(|t| matches!(t, ToolSpec::FileSearch { .. })),
        web_search: tools
            .iter()
            .any(|t| matches!(t, ToolSpec::WebSearch { .. })),
        code_interpreter: tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. })),
    }
}

/// The preflight columns of a turn from its preflight decision.
pub(super) fn turn_preflight(decision: &PreflightDecision) -> TurnPreflight {
    TurnPreflight {
        reserve_tokens: decision.reserve_tokens,
        max_output_tokens_applied: to_i32(decision.max_output_tokens_applied),
        reserved_credits_micro: decision.reserved_credits_micro,
        policy_version_applied: i64::try_from(decision.snapshot.policy_version).unwrap_or(i64::MAX),
        effective_model: decision.effective_model.id.clone(),
        minimal_generation_floor_applied: to_i32(decision.minimal_generation_floor_applied),
    }
}

fn to_i32(v: i64) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// The rows the reserve transaction writes for a new send.
pub(super) struct NewSend {
    pub reserve: ReserveRequest,
    pub message: NewMessage,
    pub attachment_ids: Vec<Uuid>,
    pub turn: NewRunningTurn,
}

impl NewSend {
    /// The user message of the turn.
    pub fn user_message(
        chat: &chat::Model,
        request_id: Uuid,
        content: &str,
        now: OffsetDateTime,
    ) -> NewMessage {
        NewMessage {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id,
            role: MessageRole::User,
            content: content.to_owned(),
            request_kind: REQUEST_KIND_CHAT.to_owned(),
            features_used: serde_json::json!([]),
            provider_response_id: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: None,
            created_at: now,
        }
    }
}

/// The reserve transaction (DESIGN section 3.6): quota reserve with the limit
/// re-check, user message, attachment validation (tenant, uploader = caller,
/// chat, `ready`, not deleted) and links, running turn, `chats.updated_at`.
/// Writes come first (Ruling R5). A unique violation is re-read outside the
/// failed transaction: a turn with the same request id is
/// `RequestIdConflict`, anything else `TurnAlreadyRunning`.
///
/// # Errors
/// `QuotaExceeded`, `InvalidAttachment`, `RequestIdConflict`,
/// `TurnAlreadyRunning`, database failure. On error nothing is committed.
pub(super) async fn commit_send(
    db: &DBProvider<DomainError>,
    quota: &Arc<QuotaService>,
    send: NewSend,
) -> Result<(), DomainError> {
    let (tenant_id, chat_id, request_id) =
        (send.turn.tenant_id, send.turn.chat_id, send.turn.request_id);
    let quota = Arc::clone(quota);
    let res = db
        .transaction(move |tx| {
            Box::pin(async move {
                let now = send.turn.now;
                quota.reserve_in_tx(tx, &send.reserve).await?;
                let message_id = send.message.id;
                message_repo::insert(tx, send.message).await?;
                validate_attachments(tx, &send.turn, &send.attachment_ids).await?;
                attachment_repo::link_to_message(
                    tx,
                    tenant_id,
                    chat_id,
                    message_id,
                    &send.attachment_ids,
                    now,
                )
                .await?;
                turn_repo::insert_running(tx, send.turn).await?;
                chat_repo::touch_updated_at(tx, tenant_id, chat_id, now).await?;
                Ok(())
            })
        })
        .await;
    match res {
        Err(DomainError::UniqueViolation) => {
            let conn = db.conn()?;
            if turn_repo::find_by_request(&conn, tenant_id, chat_id, request_id)
                .await?
                .is_some()
            {
                Err(DomainError::RequestIdConflict)
            } else {
                Err(DomainError::TurnAlreadyRunning)
            }
        }
        other => other,
    }
}

/// Every requested attachment is live, in the chat (and tenant), uploaded by
/// the requester and `ready`.
async fn validate_attachments(
    runner: &impl DBRunner,
    turn: &NewRunningTurn,
    ids: &[Uuid],
) -> Result<(), DomainError> {
    let (tenant, uploader) = (turn.tenant_id, turn.requester_user_id);
    let rows = attachment_repo::live_in_chat_by_ids(runner, tenant, turn.chat_id, ids).await?;
    let valid = rows.len() == ids.len()
        && rows.iter().all(|a| {
            a.tenant_id == tenant
                && a.uploaded_by_user_id == uploader
                && a.status == AttachmentStatus::Ready.as_str()
        });
    if valid {
        Ok(())
    } else {
        Err(DomainError::InvalidAttachment)
    }
}

/// `chat_turns.error_code` of a retry/edit turn whose setup failed after the
/// mutation commit (DESIGN section 3.6 retry/edit variant, section 5.7):
/// the context budget, the reserve's limit re-check, anything else.
pub(super) fn unstarted_error_code(e: &DomainError) -> &'static str {
    match e {
        DomainError::ContextBudgetExceeded => "context_length_exceeded",
        DomainError::QuotaExceeded { .. } => "quota_exceeded",
        _ => "turn_setup_failed",
    }
}

/// The quota reserve of a turn from its preflight decision.
pub(super) fn reserve_request(
    chat: &chat::Model,
    user_id: Uuid,
    decision: &PreflightDecision,
) -> ReserveRequest {
    ReserveRequest {
        tenant_id: chat.tenant_id,
        user_id,
        premium: decision.premium(),
        reserved_credits_micro: decision.reserved_credits_micro,
        periods: decision.periods,
        limits: decision.limits,
    }
}

/// The reserve transaction of a retry/edit turn (DESIGN section 3.7,
/// "Preflight columns on retry/edit"): the quota reserve with the limit
/// re-check, then the turn's preflight columns (`reserve_tokens IS NULL`
/// guard: written once). Writes only (Ruling R5).
///
/// # Errors
/// `QuotaExceeded`; `Internal` when the turn is no longer an unstarted
/// running turn; database failure. On error nothing is committed.
pub(super) async fn commit_reserve(
    db: &DBProvider<DomainError>,
    quota: &Arc<QuotaService>,
    reserve: &ReserveRequest,
    turn_id: Uuid,
    preflight: TurnPreflight,
) -> Result<(), DomainError> {
    let quota = Arc::clone(quota);
    let reserve = *reserve;
    db.transaction(move |tx| {
        Box::pin(async move {
            quota.reserve_in_tx(tx, &reserve).await?;
            if turn_repo::fill_preflight(tx, reserve.tenant_id, turn_id, &preflight).await? == 0 {
                return Err(DomainError::Internal(format!(
                    "turn {turn_id} is not an unstarted running turn"
                )));
            }
            Ok(())
        })
    })
    .await
}
