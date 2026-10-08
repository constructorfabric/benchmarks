//! Send pipeline up to the reserve transaction (DESIGN §3.6 "Send Message with Streaming Response").
//! Every fallible pre-provider step runs before the reserve, so a rejection leaves nothing behind.

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use toolkit_db::secure::secure_insert;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::chats;
use crate::domain::context::{self, ContextPlan, ContextRequest};
use crate::domain::error::{DomainError, Resource};
use crate::domain::models;
use crate::domain::quota::{self, PreflightDecision, PreflightRequest, ToolInputs};
use crate::domain::services::AppServices;
use crate::domain::stream::events::StreamEvent;
use crate::domain::stream::queries::{self, ChatAttachmentState, STATE_COMPLETED, STATE_RUNNING};
use crate::domain::stream::replay;
use crate::domain::summary::SummaryTrigger;
use crate::infra::db::entities::{attachment, chat, chat_turn, message};
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::responses::{self, ChatRequest, RequestMetadata, ToolSpec};

/// Input of `messages:stream`.
#[derive(Debug, Clone)]
pub struct SendInput {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}

/// A turn ready to stream: reserve committed, `running` turn and user message persisted.
#[derive(Debug, Clone)]
pub struct LiveTurn {
    pub ctx: SecurityContext,
    pub chat: chat::Model,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// Pre-allocated assistant message id.
    pub message_id: Uuid,
    pub decision: PreflightDecision,
    pub provider: ResolvedProvider,
    pub request_body: serde_json::Value,
    pub summary_applied: Option<i64>,
    pub summary_trigger: SummaryTrigger,
    pub file_map: std::collections::HashMap<String, (Uuid, String)>,
    pub started: Instant,
}

/// Outcome of the setup of a streaming request.
#[derive(Debug)]
pub enum StartOutcome {
    /// Idempotent replay of a completed turn (side-effect free).
    Replay(Vec<StreamEvent>),
    /// New generation.
    Live(Box<LiveTurn>),
}

/// Preflight result shared by send and retry/edit.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub decision: PreflightDecision,
    pub attach: ChatAttachmentState,
    /// Provider file ids of the images sent with the message.
    pub image_file_ids: Vec<String>,
}

/// 400 `EMPTY_CONTENT` for empty / whitespace-only content.
///
/// # Errors
/// `EMPTY_CONTENT`.
pub fn validate_content(content: &str) -> Result<(), DomainError> {
    if content.trim().is_empty() {
        return Err(DomainError::invalid(Resource::Chat, "content", "EMPTY_CONTENT", "content must not be empty"));
    }
    Ok(())
}

/// Preflight: chat model (INVALID_MODEL), quota cascade, image guards, input limit.
///
/// # Errors
/// JSON errors of the preflight.
pub async fn run_preflight(
    app: &AppServices,
    ctx: &SecurityContext,
    chat: &chat::Model,
    content: &str,
    images: &[attachment::Model],
    web_search: bool,
) -> Result<Preflight, DomainError> {
    let tenant = ctx.subject_tenant_id();
    let user = ctx.subject_id();
    let snapshot = app.policy.current_snapshot(user).await?;
    models::chat_model(&snapshot, &chat.model)?;

    let conn = app.db.conn()?;
    let attach = queries::chat_attachment_state(&conn, tenant, chat.id).await?;
    let prior = queries::prior_context_tokens(&conn, tenant, chat.id).await?;
    drop(conn);

    let image_count = u32::try_from(images.len()).unwrap_or(u32::MAX);
    let decision = quota::preflight(
        app,
        &PreflightRequest {
            tenant_id: tenant,
            user_id: user,
            selected_model: chat.model.clone(),
            message_bytes: content.len(),
            image_count,
            prior_context_tokens: prior,
            tools: ToolInputs {
                has_ready_documents: attach.has_ready_documents,
                has_ready_code_interpreter: attach.has_ready_code_interpreter,
                web_search_requested: web_search,
            },
            now: clock::now(),
        },
    )
    .await?;

    if !images.is_empty() {
        if decision.snapshot.kill_switches.disable_images {
            return Err(DomainError::feature_disabled("images"));
        }
        if image_count > app.cfg.rag.max_images_per_message {
            return Err(DomainError::out_of_range(
                Resource::Chat,
                "image_count",
                "TOO_MANY_IMAGES",
                format!("at most {} images per message", app.cfg.rag.max_images_per_message),
            ));
        }
        if !decision.effective_model.supports_vision() {
            return Err(DomainError::invalid(
                Resource::Chat,
                "content_type",
                "VISION_NOT_SUPPORTED",
                format!("model '{}' does not support image input", decision.effective_model.id),
            ));
        }
    }
    let model = &decision.effective_model;
    if model.max_input_tokens > 0
        && quota::estimate_text_tokens(content.len(), &model.estimation_budgets) > i64::from(model.max_input_tokens)
    {
        return Err(DomainError::out_of_range(
            Resource::Chat,
            "content",
            "INPUT_TOO_LONG",
            format!("message exceeds the model input limit of {} tokens", model.max_input_tokens),
        ));
    }
    let image_file_ids = images.iter().filter_map(|a| a.provider_file_id.clone()).collect();
    Ok(Preflight { decision, attach, image_file_ids })
}

/// Provider request plan (context, tools, body).
#[derive(Debug, Clone)]
pub struct ProviderPlan {
    pub provider: ResolvedProvider,
    pub body: serde_json::Value,
    pub plan: ContextPlan,
    pub summary_exists: bool,
}

fn feature_label(tools: &[ToolSpec]) -> String {
    let mut parts = Vec::new();
    if tools.iter().any(|t| matches!(t, ToolSpec::FileSearch { .. })) {
        parts.push("file_search");
    }
    if tools.iter().any(|t| matches!(t, ToolSpec::WebSearch { .. })) {
        parts.push("web_search");
    }
    if tools.iter().any(|t| matches!(t, ToolSpec::CodeInterpreter { .. })) {
        parts.push("code_interpreter");
    }
    if parts.is_empty() { "none".to_owned() } else { parts.join("+") }
}

/// Assembles the context and builds the provider request for the effective model.
///
/// # Errors
/// `CONTEXT_BUDGET_EXCEEDED`, provider resolution (500) or DB errors.
pub async fn build_provider_plan(
    app: &AppServices,
    ctx: &SecurityContext,
    chat: &chat::Model,
    pf: &Preflight,
    content: &str,
    boundary: Option<(OffsetDateTime, Uuid)>,
    exclude_message_id: Option<Uuid>,
) -> Result<ProviderPlan, DomainError> {
    let tenant = ctx.subject_tenant_id();
    let model: &ModelCatalogEntry = &pf.decision.effective_model;
    let budgets = &model.estimation_budgets;
    let mut tools = Vec::new();
    let mut guards = Vec::new();
    let mut surcharge: i64 = 0;
    if pf.decision.tools.file_search {
        surcharge += i64::from(budgets.tool_surcharge_tokens);
        guards.push(app.cfg.context.file_search_guard.clone());
        if let Some(vs) = &pf.attach.vector_store_id {
            tools.push(ToolSpec::FileSearch { vector_store_ids: vec![vs.clone()], max_num_results: model.max_num_results });
        }
    }
    if pf.decision.tools.web_search {
        surcharge += i64::from(budgets.web_search_surcharge_tokens);
        guards.push(app.cfg.context.web_search_guard.clone());
        tools.push(ToolSpec::WebSearch { context_size: model.web_search_context_size });
    }
    if pf.decision.tools.code_interpreter {
        surcharge += i64::from(budgets.code_interpreter_surcharge_tokens);
        tools.push(ToolSpec::CodeInterpreter { file_ids: pf.attach.code_interpreter_file_ids.clone() });
    }

    let (summary, recent) = context::load_history(
        app,
        tenant,
        chat.id,
        boundary,
        exclude_message_id,
        app.cfg.context.recent_messages_limit,
    )
    .await?;
    let summary_exists = summary.is_some();
    let plan = context::assemble(&ContextRequest {
        model: model.clone(),
        max_output_tokens_applied: pf.decision.max_output_tokens_applied,
        guards,
        summary,
        recent,
        user_message: content.to_owned(),
        image_file_ids: pf.image_file_ids.clone(),
        surcharge_tokens: surcharge,
    })?;
    let provider = app.providers.resolve(&model.provider_id, tenant)?;
    let feature = feature_label(&tools);
    let req = ChatRequest {
        model: model.provider_model_id.clone(),
        instructions: plan.instructions.clone(),
        input: plan.input.clone(),
        max_output_tokens: pf.decision.max_output_tokens_applied,
        tools,
        user: responses::provider_user_field(tenant, ctx.subject_id()),
        metadata: RequestMetadata {
            tenant_id: tenant.to_string(),
            user_id: ctx.subject_id().to_string(),
            chat_id: chat.id.to_string(),
            request_type: "chat".to_owned(),
            feature,
        },
        api_params: model.general_config.api_params.clone(),
        max_tool_calls: model.max_tool_calls,
        stream: true,
    };
    let body = responses::build_request_body(provider.kind, &req);
    Ok(ProviderPlan { provider, body, plan, summary_exists })
}

/// Summary trigger inputs of a plan.
#[must_use]
pub fn summary_trigger(p: &ProviderPlan) -> SummaryTrigger {
    SummaryTrigger {
        messages_truncated: p.plan.messages_truncated,
        assembled_tokens: p.plan.assembled_tokens,
        effective_budget: p.plan.effective_budget,
        summary_exists: p.summary_exists,
    }
}

/// Maps a unique violation raised while inserting a turn to the 409 contract.
async fn map_insert_conflict(app: &AppServices, tenant: Uuid, chat_id: Uuid, request_id: Uuid, err: DomainError) -> DomainError {
    if !err.is_unique_violation() {
        return err;
    }
    let existing = match app.db.conn() {
        Ok(conn) => queries::find_turn(&conn, tenant, chat_id, request_id).await.ok().flatten(),
        Err(_) => None,
    };
    if existing.is_some() {
        request_id_conflict()
    } else {
        turn_already_running()
    }
}

#[must_use]
pub fn request_id_conflict() -> DomainError {
    DomainError::aborted(Resource::Chat, "request_id_conflict", "The request_id was already used for another turn")
}

#[must_use]
pub fn turn_already_running() -> DomainError {
    DomainError::aborted(Resource::Chat, "turn_already_running", "A generation is already running for this chat")
}

/// Full setup of `messages:stream`.
///
/// # Errors
/// JSON errors (validation, authz, idempotency, parallel guard, preflight, context, reserve).
#[allow(clippy::too_many_lines)]
pub async fn start_send(
    app: &Arc<AppServices>,
    ctx: &SecurityContext,
    chat_id: Uuid,
    input: SendInput,
) -> Result<StartOutcome, DomainError> {
    let started = Instant::now();
    validate_content(&input.content)?;
    let max_ids = usize::try_from(app.cfg.rag.max_documents_per_chat.saturating_add(app.cfg.rag.max_images_per_message))
        .unwrap_or(usize::MAX);
    if input.attachment_ids.len() > max_ids {
        return Err(queries::invalid_attachment("too many attachment ids"));
    }
    let mut seen = std::collections::HashSet::new();
    if !input.attachment_ids.iter().all(|id| seen.insert(*id)) {
        return Err(queries::invalid_attachment("duplicate attachment id"));
    }

    let tenant = ctx.subject_tenant_id();
    let user = ctx.subject_id();
    let scope = app.authz.chat_scope(ctx, "send_message", Some(chat_id)).await?;
    let chat = chats::load_chat(app, &scope, chat_id).await?;
    let request_id = input.request_id.unwrap_or_else(Uuid::new_v4);

    // 1. Idempotency (before the parallel-turn guard).
    let conn = app.db.conn()?;
    if let Some(existing) = queries::find_turn(&conn, tenant, chat_id, request_id).await? {
        if existing.state == STATE_COMPLETED && existing.deleted_at.is_none() {
            return Ok(StartOutcome::Replay(replay::replay_events(app, &chat, &existing).await?));
        }
        return Err(request_id_conflict());
    }
    // 2. Parallel turn guard.
    if queries::has_running_turn(&conn, tenant, chat_id).await? {
        return Err(turn_already_running());
    }
    let boundary = queries::snapshot_boundary(&conn, tenant, chat_id).await?;
    let referenced = queries::attachments_by_ids(&conn, tenant, chat_id, &input.attachment_ids).await?;
    drop(conn);
    let images: Vec<attachment::Model> = referenced.into_iter().filter(|a| a.attachment_kind == "image").collect();

    // 3. Preflight, context, provider.
    let pf = run_preflight(app, ctx, &chat, &input.content, &images, input.web_search).await?;
    let plan = build_provider_plan(app, ctx, &chat, &pf, &input.content, boundary, None).await?;

    // 4. Reserve transaction.
    let turn_id = Uuid::new_v4();
    let user_message_id = Uuid::new_v4();
    let web_search = input.web_search;
    let res = crate::domain::tx::retry_contention(|| {
        let decision = pf.decision.clone();
        let content = input.content.clone();
        let attachment_ids = input.attachment_ids.clone();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                quota::reserve(tx, tenant, user, &decision).await?;
                let scope = AccessScope::for_tenant(tenant);
                let msg = message::ActiveModel {
                    id: Set(user_message_id),
                    tenant_id: Set(tenant),
                    chat_id: Set(chat_id),
                    request_id: Set(Some(request_id)),
                    role: Set("user".to_owned()),
                    content: Set(content),
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
                    created_at: Set(now),
                    deleted_at: Set(None),
                };
                secure_insert::<message::Entity>(msg, &scope, tx).await?;
                queries::validate_and_link_attachments(tx, tenant, user, chat_id, user_message_id, &attachment_ids, now).await?;
                chats::touch_chat(tx, tenant, chat_id, now).await?;
                let turn = new_turn_model(turn_id, tenant, chat_id, request_id, user, web_search, now, Some(&decision));
                secure_insert::<chat_turn::Entity>(turn, &scope, tx).await?;
                Ok(())
            })
        })
    })
    .await;
    if let Err(e) = res {
        return Err(map_insert_conflict(app, tenant, chat_id, request_id, e).await);
    }

    Ok(StartOutcome::Live(Box::new(LiveTurn {
        ctx: ctx.clone(),
        chat,
        turn_id,
        request_id,
        message_id: Uuid::new_v4(),
        summary_applied: plan.plan.summary_applied,
        summary_trigger: summary_trigger(&plan),
        decision: pf.decision,
        provider: plan.provider,
        request_body: plan.body,
        file_map: pf.attach.file_map,
        started,
    })))
}

/// New `running` turn row; reserve columns set when a decision is given (send path).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn new_turn_model(
    turn_id: Uuid,
    tenant: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    user: Uuid,
    web_search: bool,
    now: OffsetDateTime,
    decision: Option<&PreflightDecision>,
) -> chat_turn::ActiveModel {
    chat_turn::ActiveModel {
        id: Set(turn_id),
        tenant_id: Set(tenant),
        chat_id: Set(chat_id),
        request_id: Set(request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(Some(user)),
        state: Set(STATE_RUNNING.to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(decision.map(|d| d.reserve_tokens)),
        max_output_tokens_applied: Set(decision.map(|d| i32::try_from(d.max_output_tokens_applied).unwrap_or(i32::MAX))),
        reserved_credits_micro: Set(decision.map(|d| d.reserved_credits_micro)),
        policy_version_applied: Set(decision.map(|d| d.policy_version)),
        effective_model: Set(decision.map(|d| d.effective_model.id.clone())),
        minimal_generation_floor_applied: Set(
            decision.map(|d| i32::try_from(d.minimal_generation_floor_applied).unwrap_or(i32::MAX)),
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
