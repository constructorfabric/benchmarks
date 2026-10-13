//! Turn setup of a send (DESIGN "Send Message with Streaming Response", "Check Priority Order")
//! and of a retry/edit ([`prepare_mutation`], DESIGN 3.9).
//!
//! Send: every step before the reserve transaction only reads; a rejection is returned as an
//! error (a JSON problem, no stream) and leaves nothing behind. The reserve transaction books the
//! quota reserve, persists the user message and inserts the `running` turn atomically.
//!
//! Retry/edit: the same read-only checks run before the mutation commit (which inserts the user
//! message and the `running` turn); context assembly, provider target and the reserve run after
//! it, and a failure there moves the new turn to `failed`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, PolicySnapshot};
use sea_orm::ActiveValue::{NotSet, Set};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::knowledge::{self, KnowledgeParams};
use super::replay::{self, Replay};
use super::{
    MutationPlan, Replacement, SendMessage, StreamService, SummaryTriggerInfo, TurnContext,
};
use crate::config::RagConfig;
use crate::domain::authz::ChatAction;
use crate::domain::context::{ContextInput, ContextPlan, HistoryMessage, assemble};
use crate::domain::error::DomainError;
use crate::domain::quota::estimate::estimate_text_tokens;
use crate::domain::quota::{PreflightDecision, PreflightInput, QuotaService, ToolGates};
use crate::domain::tools::{ToolSet, build_tools};
use crate::domain::turn_service::{MutationCommit, MutationOp, NewTurn, commit_mutation};
use crate::infra::db::entity::{attachments, chat_turns, chats, messages};
use crate::infra::db::repo::messages::Position;
use crate::infra::db::repo::turns::{PreflightColumns, TurnTerminal};
use crate::infra::db::repo::{
    attachments as attachment_repo, chats as chat_repo, messages as message_repo, thread_summaries,
    turns, vector_stores,
};
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx;
use crate::infra::db::{AttachmentKind, AttachmentStatus, MessageRole, TurnState};
use crate::infra::llm::{
    ChatAdapter, ContentPart, ProviderRequest, RequestMetadata, Role, ToolSpec,
};

/// What a send does: replay a completed turn, or stream a new one.
pub enum SendPlan {
    Replay(Replay),
    Live(Box<LiveTurn>),
}

/// A committed `running` turn, ready for its provider call.
pub struct LiveTurn {
    pub turn: TurnContext,
    pub adapter: Arc<dyn ChatAdapter>,
    pub request: ProviderRequest,
}

/// Attachment facts of the chat and of the referenced attachments (setup step 6).
struct AttachmentFacts {
    /// Ready, non-deleted attachments of the chat.
    ready: Vec<attachments::Model>,
    /// Images among the referenced attachments of this chat, in request order.
    images: Vec<ContentPart>,
    image_count: u32,
}

impl AttachmentFacts {
    fn has_ready_documents(&self) -> bool {
        self.ready
            .iter()
            .any(|a| a.attachment_kind == AttachmentKind::Document.as_str() && a.for_file_search)
    }

    /// Provider file ids of the ready code-interpreter files.
    fn ci_file_ids(&self) -> Vec<String> {
        self.ready
            .iter()
            .filter(|a| a.for_code_interpreter)
            .filter_map(|a| a.provider_file_id.clone())
            .collect()
    }

    /// `provider_file_id -> (attachment_id, filename)` of the ready attachments.
    fn citation_map(&self) -> HashMap<String, (Uuid, String)> {
        self.ready
            .iter()
            .filter_map(|a| {
                let file = a.provider_file_id.clone()?;
                Some((file, (a.id, a.filename.clone())))
            })
            .collect()
    }
}

/// The assembled provider input and tools (setup step 10).
struct Assembled {
    tools: ToolSet,
    plan: ContextPlan,
    summary_exists: bool,
    file_citation_map: HashMap<String, (Uuid, String)>,
    knowledge: Option<KnowledgeParams>,
}

/// Runs the setup of a send in the normative order.
///
/// # Errors
/// `EmptyContent`, `InvalidAttachment`, PDP errors, `ChatNotFound`, `RequestIdConflict`,
/// `TurnAlreadyRunning`, `InvalidModel`, `TooManyImages`, `FeatureDisabled`,
/// `QuotaExceeded`, `VisionNotSupported`, `InputTooLong`, `ContextBudgetExceeded`, `Internal`.
pub async fn prepare(
    svc: &StreamService,
    ctx: &SecurityContext,
    chat_id: Uuid,
    req: SendMessage,
) -> Result<SendPlan, DomainError> {
    validate_request(&svc.cfg.rag, &req)?;
    let scope = svc
        .authz
        .chat_scope(ctx, ChatAction::SendMessage, Some(chat_id))
        .await?;
    let tenant_scope = scope.tenant_only();
    let conn = svc.db.conn()?;
    let chat = chat_repo::load_scoped(&conn, &scope, chat_id)
        .await?
        .ok_or_else(|| DomainError::ChatNotFound {
            id: chat_id.to_string(),
        })?;
    // Idempotency and the parallel-turn guard come before model resolution (DESIGN "Check
    // Priority Order"): a replay needs nothing from the policy snapshot and must not fail when
    // the model left the catalog or the policy plugin is down.
    let request_id = req.request_id.unwrap_or_else(Uuid::new_v4);
    if let Some(replay) = check_turn_slot(&conn, &tenant_scope, &chat, request_id).await? {
        return Ok(SendPlan::Replay(replay));
    }
    let (snapshot, _) = svc.models.resolve_chat_model(ctx, &chat.model).await?;

    let boundary = message_repo::latest_position(&conn, &tenant_scope, chat_id).await?;
    let checked = check_preflight(svc, ctx, &conn, &tenant_scope, &chat, &snapshot, req).await?;
    let ids = TurnIds::new(request_id);
    let (turn, assembled) = start_turn(
        svc,
        ctx,
        &conn,
        &tenant_scope,
        &chat,
        boundary,
        &checked,
        ids,
    )
    .await?;
    reserve(svc, &tenant_scope, &turn, &checked.req).await?;
    Ok(SendPlan::Live(Box::new(live_turn(svc, turn, assembled))))
}

/// Ids of a new turn and its user message.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)] // named like the `TurnContext` fields they fill
struct TurnIds {
    turn_id: Uuid,
    request_id: Uuid,
    user_message_id: Uuid,
}

impl TurnIds {
    fn new(request_id: Uuid) -> Self {
        Self {
            turn_id: Uuid::new_v4(),
            request_id,
            user_message_id: Uuid::new_v4(),
        }
    }
}

/// A message that passed the read-only checks (setup steps 6–9).
struct Checked {
    req: SendMessage,
    facts: AttachmentFacts,
    decision: PreflightDecision,
}

/// Steps 6–9, read-only (shared by send and retry/edit): attachment facts and the image count,
/// the quota preflight, the image guards and the input length.
async fn check_preflight(
    svc: &StreamService,
    ctx: &SecurityContext,
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: &chats::Model,
    snapshot: &PolicySnapshot,
    req: SendMessage,
) -> Result<Checked, DomainError> {
    let prior_context_tokens = message_repo::prior_context_tokens(conn, scope, chat.id).await?;
    let facts = attachment_facts(conn, scope, chat.id, &req.attachment_ids).await?;
    if facts.image_count > svc.cfg.rag.max_images_per_message {
        return Err(DomainError::TooManyImages);
    }
    let decision = preflight(svc, ctx, chat, snapshot, &req, prior_context_tokens, &facts).await?;
    check_images(facts.image_count, snapshot.kill_switches, &decision)?;
    check_input_length(&req.content, &decision.effective_model)?;
    Ok(Checked {
        req,
        facts,
        decision,
    })
}

/// Steps 10–11 (shared by send and retry/edit): context assembly up to `boundary` and the
/// provider target of the effective model; the turn's context.
#[allow(clippy::too_many_arguments)] // the setup's step inputs, passed through once
async fn start_turn(
    svc: &StreamService,
    ctx: &SecurityContext,
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: &chats::Model,
    boundary: Option<Position>,
    checked: &Checked,
    ids: TurnIds,
) -> Result<(TurnContext, Assembled), DomainError> {
    let Checked {
        req,
        facts,
        decision,
    } = checked;
    let mut assembled =
        assemble_context(svc, conn, scope, chat, boundary, decision, facts, req).await?;
    let target = svc
        .providers
        .chat_target(&decision.effective_model.provider_id, chat.tenant_id)?;
    let turn = TurnContext {
        turn_id: ids.turn_id,
        chat_id: chat.id,
        tenant_id: chat.tenant_id,
        user_id: ctx.subject_id(),
        request_id: ids.request_id,
        user_message_id: ids.user_message_id,
        assistant_message_id: Uuid::new_v4(),
        selected_model: chat.model.clone(),
        decision: decision.clone(),
        target,
        provider_id: decision.effective_model.provider_id.clone(),
        web_search_enabled: req.web_search_enabled,
        file_citation_map: std::mem::take(&mut assembled.file_citation_map),
        summary: SummaryTriggerInfo {
            assembled_tokens: assembled.plan.assembled_tokens,
            effective_budget: assembled.plan.effective_budget,
            messages_truncated: assembled.plan.messages_truncated,
            summary_exists: assembled.summary_exists,
        },
        thread_summary_applied: assembled.plan.summary_applied,
        knowledge: assembled.knowledge.take(),
        started_at: Instant::now(),
    };
    Ok((turn, assembled))
}

/// The provider request and adapter of a committed turn.
fn live_turn(svc: &StreamService, turn: TurnContext, assembled: Assembled) -> LiveTurn {
    let request = provider_request(&turn, assembled.plan, assembled.tools);
    let adapter = svc.llm.adapter(turn.target.kind);
    LiveTurn {
        turn,
        adapter,
        request,
    }
}

/// Retry or edit of the latest turn (DESIGN 1540–1548, 3.9), after the turn service's read-only
/// preview:
/// 1. preflight of the re-submitted message, read-only: chat model, the original user message
///    and its non-deleted attachments (images are re-sent), `web_search_enabled` of the replaced
///    turn, quota preflight, image guards, input length — a rejection changes nothing;
/// 2. the mutation commit ([`commit_mutation`]): the target is replaced by a new `running` turn
///    without preflight columns;
/// 3. context assembly, provider target and the reserve transaction, which also fills the
///    preflight columns. A failure here moves the new turn to `failed` (plain CAS, no
///    settlement, no outbox event) and is returned.
///
/// # Errors
/// `InvalidModel`, `TooManyImages`, `FeatureDisabled`, `QuotaExceeded`, `VisionNotSupported`,
/// `InputTooLong` (nothing changed); the commit's `NotLatestTurn`, `TurnNotTerminal`,
/// `GenerationInProgress`; after the commit `ContextBudgetExceeded`, `QuotaExceeded` or any
/// other setup error; `Internal`.
pub async fn prepare_mutation(
    svc: &StreamService,
    ctx: &SecurityContext,
    chat: chats::Model,
    plan: MutationPlan,
) -> Result<LiveTurn, DomainError> {
    let MutationPlan {
        replacement,
        scope,
        target,
    } = plan;
    let conn = svc.db.conn()?;
    let (snapshot, _) = svc.models.resolve_chat_model(ctx, &chat.model).await?;
    let original =
        message_repo::find_of_turn(&conn, &scope, chat.id, target.request_id, MessageRole::User)
            .await?
            .ok_or_else(|| {
                DomainError::Internal(format!("turn {} has no user message", target.request_id))
            })?;
    let attachment_ids =
        attachment_repo::summaries_for_messages(&conn, &scope, chat.id, &[original.id])
            .await?
            .remove(&original.id)
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.id)
            .collect();
    let (op, content) = match replacement {
        Replacement::Retry => (MutationOp::Retry, original.content),
        Replacement::Edit(content) => (MutationOp::Edit, content),
    };
    let ids = TurnIds::new(Uuid::new_v4());
    let req = SendMessage {
        content,
        request_id: Some(ids.request_id),
        attachment_ids,
        web_search_enabled: target.web_search_enabled,
    };
    let checked = check_preflight(svc, ctx, &conn, &scope, &chat, &snapshot, req).await?;

    commit_mutation(
        &svc.db,
        &svc.outbox,
        MutationCommit {
            op,
            scope: scope.clone(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            actor: ctx.subject_id(),
            target,
            new_turn: Some(NewTurn {
                turn_id: ids.turn_id,
                request_id: ids.request_id,
                user_message_id: ids.user_message_id,
                content: checked.req.content.clone(),
                attachment_ids: checked.req.attachment_ids.clone(),
                web_search_enabled: checked.req.web_search_enabled,
            }),
        },
    )
    .await?;

    match start_replacement(svc, ctx, &conn, &scope, &chat, &checked, ids).await {
        Ok(live) => Ok(live),
        Err(err) => {
            fail_unstarted(svc, &scope, ids.turn_id, &err).await;
            Err(err)
        }
    }
}

/// Post-commit setup of a retry/edit turn: the history up to the latest message before the new
/// user message, the provider target and the reserve.
async fn start_replacement(
    svc: &StreamService,
    ctx: &SecurityContext,
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: &chats::Model,
    checked: &Checked,
    ids: TurnIds,
) -> Result<LiveTurn, DomainError> {
    let boundary =
        message_repo::latest_position_excluding(conn, scope, chat.id, ids.request_id).await?;
    let (turn, assembled) = start_turn(svc, ctx, conn, scope, chat, boundary, checked, ids).await?;
    reserve_replacement(svc, scope, &turn).await?;
    Ok(live_turn(svc, turn, assembled))
}

/// The reserve transaction of a retry/edit turn: the quota reserve (with the limit re-check) is
/// the first write, then the preflight columns of the `running` turn.
async fn reserve_replacement(
    svc: &StreamService,
    scope: &AccessScope,
    turn: &TurnContext,
) -> Result<(), DomainError> {
    let quota: Arc<QuotaService> = Arc::clone(&svc.quota);
    let (scope_tx, turn_tx) = (scope.clone(), turn.clone());
    let facts = write_tx(&svc.db, move |tx| {
        let (scope, turn, quota) = (scope_tx.clone(), turn_tx.clone(), Arc::clone(&quota));
        Box::pin(async move {
            let facts = quota
                .reserve(tx, turn.tenant_id, turn.user_id, &turn.decision)
                .await?;
            if !turns::fill_preflight(tx, &scope, turn.turn_id, &preflight_columns(&turn)).await? {
                return Err(DomainError::Internal(
                    "the retry/edit turn is no longer running".to_owned(),
                ));
            }
            Ok(facts)
        })
    })
    .await?;
    // Metrics only after the commit: the closure may have run several times or rolled back.
    svc.quota.record_facts(facts);
    Ok(())
}

fn preflight_columns(turn: &TurnContext) -> PreflightColumns {
    let reserve = &turn.decision.reserve;
    PreflightColumns {
        reserve_tokens: reserve.reserve_tokens,
        max_output_tokens_applied: reserve.max_output_tokens_applied,
        reserved_credits_micro: reserve.reserved_credits_micro,
        policy_version_applied: turn.decision.policy_version,
        effective_model: turn.decision.effective_model.id.clone(),
        minimal_generation_floor_applied: reserve.minimal_generation_floor_applied,
    }
}

/// `chat_turns.error_code` of a retry/edit turn whose post-commit setup failed with `err`.
fn setup_failure_code(err: &DomainError) -> &'static str {
    match err {
        DomainError::ContextBudgetExceeded => "context_length_exceeded",
        DomainError::QuotaExceeded { .. } => "quota_exceeded",
        _ => "turn_setup_failed",
    }
}

/// Moves the unstarted retry/edit turn `turn_id` to `failed` with a plain CAS (DESIGN 5.7
/// "Unstarted retry/edit turn"): no reserve exists, so no settlement and no outbox event. A
/// failure is logged; the orphan watchdog finalizes a turn left `running`.
async fn fail_unstarted(
    svc: &StreamService,
    scope: &AccessScope,
    turn_id: Uuid,
    err: &DomainError,
) {
    let code = setup_failure_code(err);
    let problem = match mark_failed(svc, scope, turn_id, code).await {
        Ok(true) => return,
        Ok(false) => "the turn was already finalized".to_owned(),
        Err(e) => e.to_string(),
    };
    tracing::warn!(%turn_id, error_code = code, setup_error = %err, problem, "could not mark the retry/edit turn failed");
}

async fn mark_failed(
    svc: &StreamService,
    scope: &AccessScope,
    turn_id: Uuid,
    code: &'static str,
) -> Result<bool, DomainError> {
    let scope = scope.clone();
    write_tx(&svc.db, move |tx| {
        let scope = scope.clone();
        Box::pin(async move {
            let terminal = TurnTerminal {
                state: TurnState::Failed,
                error_code: Some(code.to_owned()),
                error_detail: None,
                provider_response_id: None,
                assistant_message_id: None,
                now: db_now(),
            };
            turns::finalize_running(tx, &scope, turn_id, &terminal).await
        })
    })
    .await
}

/// Step 1: non-blank content, unique attachment ids, at most `max_documents_per_chat +
/// max_images_per_message` of them.
fn validate_request(rag: &RagConfig, req: &SendMessage) -> Result<(), DomainError> {
    if req.content.trim().is_empty() {
        return Err(DomainError::EmptyContent);
    }
    let unique: HashSet<&Uuid> = req.attachment_ids.iter().collect();
    if unique.len() != req.attachment_ids.len() {
        return Err(DomainError::InvalidAttachment(
            "attachment_ids must be unique".to_owned(),
        ));
    }
    let max = u64::from(rag.max_documents_per_chat) + u64::from(rag.max_images_per_message);
    if req.attachment_ids.len() as u64 > max {
        return Err(DomainError::InvalidAttachment(format!(
            "at most {max} attachments per message"
        )));
    }
    Ok(())
}

/// Steps 4 and 5, in this order: a completed, non-deleted turn of `request_id` is replayed; any
/// other turn of it is a `request_id` conflict; then no other turn of the chat may be running.
async fn check_turn_slot(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: &chats::Model,
    request_id: Uuid,
) -> Result<Option<Replay>, DomainError> {
    if let Some(existing) = turns::find_by_request(conn, scope, chat.id, request_id).await? {
        if existing.state == TurnState::Completed.as_str() && existing.deleted_at.is_none() {
            return replay::load(conn, scope, chat, &existing).await.map(Some);
        }
        return Err(DomainError::RequestIdConflict);
    }
    if turns::has_running(conn, scope, chat.id).await? {
        return Err(DomainError::TurnAlreadyRunning);
    }
    Ok(None)
}

/// Step 6: the chat's ready attachments and the referenced attachments of this chat.
async fn attachment_facts(
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    attachment_ids: &[Uuid],
) -> Result<AttachmentFacts, DomainError> {
    let ready = attachment_repo::ready_in_chat(conn, scope, chat_id).await?;
    let referenced = attachment_repo::by_ids(conn, scope, attachment_ids).await?;
    let mut images = Vec::new();
    let mut image_count = 0_u32;
    for id in attachment_ids {
        let Some(a) = referenced
            .iter()
            .find(|a| a.id == *id && a.chat_id == chat_id && a.deleted_at.is_none())
        else {
            continue;
        };
        if a.attachment_kind != AttachmentKind::Image.as_str() {
            continue;
        }
        image_count = image_count.saturating_add(1);
        if let Some(file_id) = &a.provider_file_id {
            images.push(ContentPart::Image {
                file_id: file_id.clone(),
                secondary_file_id: a.secondary_file_id.clone(),
            });
        }
    }
    Ok(AttachmentFacts {
        ready,
        images,
        image_count,
    })
}

/// Step 7: the quota preflight of the chat's model.
async fn preflight(
    svc: &StreamService,
    ctx: &SecurityContext,
    chat: &chats::Model,
    snapshot: &PolicySnapshot,
    req: &SendMessage,
    prior_context_tokens: i64,
    facts: &AttachmentFacts,
) -> Result<PreflightDecision, DomainError> {
    let user_id = ctx.subject_id();
    let limits = svc
        .policy
        .user_limits(user_id, snapshot.policy_version)
        .await?;
    svc.quota
        .preflight(PreflightInput {
            tenant_id: chat.tenant_id,
            user_id,
            selected_model: chat.model.clone(),
            snapshot: snapshot.clone(),
            limits,
            message_bytes: req.content.len(),
            prior_context_tokens,
            image_count: facts.image_count,
            chat_has_ready_documents: facts.has_ready_documents(),
            chat_has_ready_ci_files: !facts.ci_file_ids().is_empty(),
            web_search_requested: req.web_search_enabled,
            streaming_max_output_tokens: svc.cfg.streaming.max_output_tokens,
            minimal_generation_floor: svc.cfg.estimation_budgets.minimal_generation_floor,
            now: OffsetDateTime::now_utc(),
        })
        .await
}

/// Step 8: images need `disable_images` off and a vision-capable effective model.
fn check_images(
    image_count: u32,
    kill_switches: KillSwitches,
    decision: &PreflightDecision,
) -> Result<(), DomainError> {
    if image_count == 0 {
        return Ok(());
    }
    if kill_switches.disable_images {
        return Err(DomainError::FeatureDisabled { subject: "images" });
    }
    if !decision.vision_supported {
        return Err(DomainError::VisionNotSupported);
    }
    Ok(())
}

/// Step 9: the message estimate (by UTF-8 bytes) within the model's `max_input_tokens`.
fn check_input_length(content: &str, model: &ModelCatalogEntry) -> Result<(), DomainError> {
    let max = i64::from(model.max_input_tokens);
    if max > 0 && estimate_text_tokens(content.len(), &model.estimation_budgets) > max {
        return Err(DomainError::InputTooLong);
    }
    Ok(())
}

/// Step 10: tools, history and thread summary, assembled for the effective model.
/// `search_knowledge` (with its guard) is added when knowledge search can serve the request and
/// `file_search` is not in the request (the two are never sent together).
#[allow(clippy::too_many_arguments)] // the setup's step inputs, passed through once
async fn assemble_context(
    svc: &StreamService,
    conn: &impl DBRunner,
    scope: &AccessScope,
    chat: &chats::Model,
    boundary: Option<Position>,
    decision: &PreflightDecision,
    facts: &AttachmentFacts,
    req: &SendMessage,
) -> Result<Assembled, DomainError> {
    let model = &decision.effective_model;
    let chat_id = chat.id;
    let ci_file_ids = facts.ci_file_ids();
    let gates = ToolGates {
        code_interpreter: decision.tools.code_interpreter && !ci_file_ids.is_empty(),
        ..decision.tools
    };
    let vector_store = if gates.file_search {
        vector_stores::vector_store_id(conn, scope, chat_id).await?
    } else {
        None
    };
    let mut tools = build_tools(
        &gates,
        model,
        vector_store.as_deref(),
        &ci_file_ids,
        &svc.cfg.context,
    );
    let has_file_search = tools
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::FileSearch { .. }));
    let file_citation_map = if has_file_search {
        facts.citation_map()
    } else {
        HashMap::new()
    };
    let knowledge = if has_file_search {
        None
    } else {
        knowledge::params(svc, chat.tenant_id)
    };
    if knowledge.is_some() {
        tools.tools.push(knowledge::tool(&svc.cfg.knowledge_search));
        tools.guards.push(svc.cfg.knowledge_search.guard.clone());
    }

    let summary = thread_summaries::find_for_chat(conn, scope, chat_id).await?;
    let history = match boundary {
        Some(boundary) => {
            let frontier = summary.as_ref().map(|s| Position {
                created_at: s.summarized_up_to_created_at,
                id: s.summarized_up_to_message_id,
            });
            let limit = u64::from(svc.cfg.context.recent_messages_limit);
            message_repo::recent(conn, scope, chat_id, boundary, frontier, limit).await?
        }
        None => Vec::new(),
    };
    let plan = assemble(ContextInput {
        model,
        max_output_tokens_applied: u32::try_from(decision.reserve.max_output_tokens_applied)
            .unwrap_or(0),
        guards: tools.guards.clone(),
        tool_surcharge_tokens: tools.surcharge_tokens,
        summary: summary
            .as_ref()
            .map(|s| (s.summary_text.clone(), i64::from(s.token_estimate))),
        history: history.into_iter().filter_map(history_message).collect(),
        current_text: req.content.clone(),
        current_images: facts.images.clone(),
    })?;
    Ok(Assembled {
        tools,
        plan,
        summary_exists: summary.is_some(),
        file_citation_map,
        knowledge,
    })
}

/// A stored user or assistant message as history (other roles are not sent).
fn history_message(m: messages::Model) -> Option<HistoryMessage> {
    let role = match MessageRole::parse(&m.role)? {
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::System => return None,
    };
    Some(HistoryMessage {
        id: m.id,
        role,
        content: m.content,
        created_at: m.created_at,
    })
}

/// Step 12: the reserve transaction. The quota reserve is the first write (`SQLite` takes the
/// write lock there). A unique violation means another turn won a race: re-read to tell a
/// `request_id` conflict from a running turn.
async fn reserve(
    svc: &StreamService,
    scope: &AccessScope,
    turn: &TurnContext,
    req: &SendMessage,
) -> Result<(), DomainError> {
    let quota: Arc<QuotaService> = Arc::clone(&svc.quota);
    let (scope_tx, turn_tx) = (scope.clone(), turn.clone());
    let (content, attachment_ids) = (req.content.clone(), req.attachment_ids.clone());
    let result = write_tx(&svc.db, move |tx| {
        let (scope, turn) = (scope_tx.clone(), turn_tx.clone());
        let (content, attachment_ids) = (content.clone(), attachment_ids.clone());
        let quota = Arc::clone(&quota);
        Box::pin(async move {
            let now = db_now();
            let facts = quota
                .reserve(tx, turn.tenant_id, turn.user_id, &turn.decision)
                .await?;
            message_repo::insert(tx, &scope, user_message(&turn, content, now)).await?;
            chat_repo::touch_updated_at(tx, turn.chat_id, now).await?;
            validate_attachments(tx, &scope, &turn, &attachment_ids).await?;
            attachment_repo::link_to_message(
                tx,
                &scope,
                turn.tenant_id,
                turn.chat_id,
                turn.user_message_id,
                &attachment_ids,
                now,
            )
            .await?;
            turns::insert(tx, &scope, running_turn(&turn, now)).await?;
            Ok(facts)
        })
    })
    .await;
    // Metrics only after the commit: the closure may have run several times or rolled back.
    let result = result.map(|facts| svc.quota.record_facts(facts));
    match result {
        Err(DomainError::Conflict {
            code: "unique_violation",
        }) => {
            let conn = svc.db.conn()?;
            let existing =
                turns::find_by_request(&conn, scope, turn.chat_id, turn.request_id).await?;
            Err(if existing.is_some() {
                DomainError::RequestIdConflict
            } else {
                DomainError::TurnAlreadyRunning
            })
        }
        other => other,
    }
}

/// Every referenced attachment: same tenant (scope), uploaded by the caller, in this chat,
/// `ready`, not deleted.
async fn validate_attachments(
    tx: &impl DBRunner,
    scope: &AccessScope,
    turn: &TurnContext,
    ids: &[Uuid],
) -> Result<(), DomainError> {
    let found = attachment_repo::by_ids(tx, scope, ids).await?;
    for id in ids {
        let valid = found.iter().any(|a| {
            a.id == *id
                && a.tenant_id == turn.tenant_id
                && a.uploaded_by_user_id == turn.user_id
                && a.chat_id == turn.chat_id
                && a.status == AttachmentStatus::Ready.as_str()
                && a.deleted_at.is_none()
        });
        if !valid {
            return Err(DomainError::InvalidAttachment(format!(
                "attachment {id} is not a ready attachment of this chat"
            )));
        }
    }
    Ok(())
}

fn user_message(turn: &TurnContext, content: String, now: OffsetDateTime) -> messages::ActiveModel {
    user_message_row(
        turn.tenant_id,
        turn.chat_id,
        turn.user_message_id,
        turn.request_id,
        content,
        now,
    )
}

/// The user message row `id` of turn `request_id` (send, retry and edit).
pub(crate) fn user_message_row(
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    request_id: Uuid,
    content: String,
    now: OffsetDateTime,
) -> messages::ActiveModel {
    messages::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(Some(request_id)),
        role: Set(MessageRole::User.as_str().to_owned()),
        content: Set(content),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(None),
    }
}

/// The `running` turn row with the preflight values (DESIGN 3.7 `chat_turns`).
fn running_turn(turn: &TurnContext, now: OffsetDateTime) -> chat_turns::ActiveModel {
    let reserve = &turn.decision.reserve;
    chat_turns::ActiveModel {
        id: Set(turn.turn_id),
        tenant_id: Set(turn.tenant_id),
        chat_id: Set(turn.chat_id),
        request_id: Set(turn.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(turn.user_id)),
        state: Set(TurnState::Running.as_str().to_owned()),
        provider_name: NotSet,
        provider_response_id: NotSet,
        assistant_message_id: NotSet,
        error_code: NotSet,
        reserve_tokens: Set(Some(reserve.reserve_tokens)),
        max_output_tokens_applied: Set(Some(reserve.max_output_tokens_applied)),
        reserved_credits_micro: Set(Some(reserve.reserved_credits_micro)),
        policy_version_applied: Set(Some(turn.decision.policy_version)),
        effective_model: Set(Some(turn.decision.effective_model.id.clone())),
        minimal_generation_floor_applied: Set(Some(reserve.minimal_generation_floor_applied)),
        error_detail: NotSet,
        deleted_at: NotSet,
        replaced_by_request_id: NotSet,
        started_at: Set(now),
        last_progress_at: Set(Some(now)),
        web_search_enabled: Set(turn.web_search_enabled),
        web_search_completed_count: NotSet,
        code_interpreter_completed_count: NotSet,
        file_search_completed_count: NotSet,
        completed_at: NotSet,
        updated_at: Set(now),
    }
}

/// The provider request of the turn (`user` = tenant hex + user hex).
fn provider_request(turn: &TurnContext, plan: ContextPlan, tools: ToolSet) -> ProviderRequest {
    let model = &turn.decision.effective_model;
    ProviderRequest {
        model: model.provider_model_id.clone(),
        instructions: plan.instructions,
        input: plan.input,
        tools: tools.tools,
        max_output_tokens: u32::try_from(turn.decision.reserve.max_output_tokens_applied)
            .unwrap_or(0),
        max_tool_calls: model.max_tool_calls,
        api_params: model.general_config.api_params.clone(),
        user: format!("{}{}", turn.tenant_id.simple(), turn.user_id.simple()),
        metadata: RequestMetadata {
            tenant_id: turn.tenant_id,
            user_id: turn.user_id,
            chat_id: Some(turn.chat_id),
            request_type: "chat",
            feature: tools.feature,
        },
        stream: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_order_and_limits() {
        let rag = RagConfig {
            max_documents_per_chat: 1,
            max_images_per_message: 1,
            ..RagConfig::default()
        };
        let req = |content: &str, ids: Vec<Uuid>| SendMessage {
            content: content.to_owned(),
            attachment_ids: ids,
            ..SendMessage::default()
        };
        assert!(matches!(
            validate_request(&rag, &req(" \n", vec![])),
            Err(DomainError::EmptyContent)
        ));
        let a = Uuid::new_v4();
        assert!(matches!(
            validate_request(&rag, &req("hi", vec![a, a])),
            Err(DomainError::InvalidAttachment(_))
        ));
        let three = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        assert!(matches!(
            validate_request(&rag, &req("hi", three)),
            Err(DomainError::InvalidAttachment(_))
        ));
        assert!(validate_request(&rag, &req("hi", vec![a, Uuid::new_v4()])).is_ok());
    }
}
