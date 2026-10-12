//! Preflight: every fallible step before the reserve transaction (DESIGN 3.6
//! "Send Message with Streaming Response"): catalog model, content, kill
//! switch, quota (downgrade cascade and daily tool quota), input length,
//! attachments and vision, tools, context budget and assembly, provider
//! resolution. A rejection here is a JSON error and writes nothing.
//!
//! The quota decision is made once, here, on the policy snapshot read at the
//! start of the preflight: the effective model it picks drives every later
//! step (input length, vision, tools, context budget, provider) and is
//! persisted by the reserve transaction.

use std::collections::HashSet;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use toolkit_db::secure::DBRunner;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{SendMessageCommand, StreamService, TurnScopes};
use crate::domain::chats::ChatRow;
use crate::domain::clock::now_utc;
use crate::domain::context::{
    ContextError, ContextInput, HistoryMessage, HistoryRole, ToolSet, check_input_length,
    plan_context_with_summary, summary_message, token_budget,
};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::messages::{ReadyAttachment, SummaryFrontier};
use crate::domain::ports::{
    AttachmentRepository, MessageMutationRepository, MessageRepository, QuotaOwner,
    QuotaRepository, RecentQuery, ThreadSummaryRepository, VectorStoreRepository,
};
use crate::domain::provider::{ChatInputItem, ChatRequest, ChatTool};
use crate::domain::quota::{
    CascadeInput, DowngradeReason, PeriodStarts, QuotaUsage, Reserve, ReserveFacts,
    estimate_reserve, resolve_effective_model,
};
use crate::domain::thread_summary::{SummaryRow, SummaryTrigger, effective_budget};

/// A provider file the `file_search` tool may cite: maps the provider file id
/// of an annotation to the attachment the client knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileTarget {
    pub(super) attachment_id: Uuid,
    pub(super) filename: String,
    pub(super) provider_file_id: String,
}

/// Everything the reserve transaction and the provider task need.
pub(super) struct TurnPlan {
    /// Catalog entry of the effective model.
    pub(super) entry: ModelCatalogEntry,
    /// The provider request.
    pub(super) request: ChatRequest,
    /// The request flag, stored on the turn even when the tool is not sent.
    pub(super) web_search_requested: bool,
    /// Citable provider files (non-empty only with `file_search`).
    pub(super) files: Vec<FileTarget>,
    /// Validated attachments to link to the user message.
    pub(super) attachment_ids: Vec<Uuid>,
    /// Policy snapshot version of the plan.
    pub(super) policy_version: u64,
    /// The reserve the effective model books (`max_output_tokens_applied` is
    /// the cap sent to the provider).
    pub(super) reserve: Reserve,
    /// Tier of the effective model: the buckets the reserve is taken from.
    pub(super) tier: ModelTier,
    /// Why the turn is downgraded, when it is.
    pub(super) downgrade_reason: Option<DowngradeReason>,
    /// The per-user limits of the policy version (re-check and warnings).
    pub(super) limits: UserLimits,
    /// The period starts the reserve and the settlement use.
    pub(super) starts: PeriodStarts,
    /// `token_estimate` of the thread summary that is part of the context.
    pub(super) summary_applied: Option<u64>,
    /// What the thread-summary trigger needs at finalization.
    pub(super) trigger: SummaryTrigger,
}

/// The provider request of a turn and what its context assembly tells the
/// thread-summary trigger.
struct Assembled {
    request: ChatRequest,
    summary_applied: Option<u64>,
    trigger: SummaryTrigger,
}

/// What the chat holds for the file tools: the ready documents and the vector
/// store (`file_search`) and the ready spreadsheets (`code_interpreter`).
struct SearchFacts {
    documents: Vec<ReadyAttachment>,
    vector_store: Option<String>,
    code_files: Vec<ReadyAttachment>,
}

impl SearchFacts {
    /// The `file_search` tool can be sent (when the model allows it).
    fn available(&self) -> bool {
        !self.documents.is_empty() && self.vector_store.is_some()
    }

    /// The `code_interpreter` tool can be sent (when the model allows it).
    fn code_available(&self) -> bool {
        self.code_files
            .iter()
            .any(|file| file.provider_file_id.is_some())
    }
}

/// The outcome of the quota gate.
struct QuotaGate {
    entry: ModelCatalogEntry,
    reserve: Reserve,
    tier: ModelTier,
    downgrade_reason: Option<DowngradeReason>,
    limits: UserLimits,
    starts: PeriodStarts,
}

/// Everything the preflight decided before the tools and the context are
/// planned: the policy snapshot, the quota decision, the guards' facts. A
/// retry / edit holds it across the mutation commit.
pub(super) struct PreflightGate {
    snapshot: PolicySnapshot,
    quota: QuotaGate,
    search: SearchFacts,
    /// Provider file ids of the images of this request.
    image_files: Vec<String>,
}

/// What the quota gate needs to know about the request.
struct QuotaFacts<'a> {
    command: &'a SendMessageCommand,
    prior_context_tokens: u64,
    image_count: u32,
    has_ready_documents: bool,
    has_code_interpreter_files: bool,
}

fn out_of_range(reason: &'static str, detail: &str) -> DomainError {
    DomainError::OutOfRange {
        resource: ResourceKind::Chat,
        field: "content",
        reason,
        detail: detail.to_owned(),
    }
}

fn context_error(error: &ContextError) -> DomainError {
    match error {
        ContextError::InputTooLong => out_of_range(
            "INPUT_TOO_LONG",
            "the message exceeds the input limit of the model",
        ),
        ContextError::BudgetExceeded => out_of_range(
            "CONTEXT_BUDGET_EXCEEDED",
            "the mandatory context does not fit the input budget of the model",
        ),
    }
}

fn count_images(attachments: &[ReadyAttachment]) -> u32 {
    let images = attachments.iter().filter(|a| a.kind == "image").count();
    u32::try_from(images).unwrap_or(u32::MAX)
}

/// The provider file ids of the images among `attachments`.
fn image_files(attachments: &[ReadyAttachment]) -> Vec<String> {
    attachments
        .iter()
        .filter(|attachment| attachment.kind == "image")
        .filter_map(|attachment| attachment.provider_file_id.clone())
        .collect()
}

/// Tools of the request: `web_search` when requested and supported (the kill
/// switch was checked before), `file_search` when the chat has a vector store
/// and a ready document and neither the model nor a kill switch forbids it.
struct ToolPlan {
    tools: Vec<ChatTool>,
    files: Vec<FileTarget>,
}

/// The tools of the request for the effective model: `file_search` when the
/// chat has a ready document and a vector store and neither the model nor a
/// kill switch forbids it, `web_search` when requested and supported,
/// `code_interpreter` with the ready spreadsheets of the chat when the model
/// supports it and its kill switch is off.
fn plan_tools(
    snapshot: &PolicySnapshot,
    entry: &ModelCatalogEntry,
    search: SearchFacts,
    command: &SendMessageCommand,
) -> ToolPlan {
    let support = &entry.general_config.tool_support;
    let file_search_allowed = support.file_search && !snapshot.kill_switches.disable_file_search;
    let store = search
        .vector_store
        .filter(|_| file_search_allowed && !search.documents.is_empty());
    let (mut tools, files) = match store {
        Some(store) => {
            let files = search
                .documents
                .into_iter()
                .filter_map(|document| {
                    document
                        .provider_file_id
                        .map(|provider_file_id| FileTarget {
                            attachment_id: document.id,
                            filename: document.filename,
                            provider_file_id,
                        })
                })
                .collect();
            let tool = ChatTool::FileSearch {
                vector_store_ids: vec![store],
                max_num_results: entry.max_num_results,
            };
            (vec![tool], files)
        }
        None => (Vec::new(), Vec::new()),
    };
    if command.web_search && support.web_search {
        tools.push(ChatTool::WebSearch {
            context_size: entry.web_search_context_size,
        });
    }
    let code_allowed = support.code_interpreter && !snapshot.kill_switches.disable_code_interpreter;
    let file_ids: Vec<String> = search
        .code_files
        .into_iter()
        .filter_map(|file| file.provider_file_id)
        .collect();
    if code_allowed && !file_ids.is_empty() {
        tools.push(ChatTool::CodeInterpreter { file_ids });
    }
    ToolPlan { tools, files }
}

impl StreamService {
    pub(super) async fn preflight<R: DBRunner>(
        &self,
        conn: &R,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &ChatRow,
        command: &SendMessageCommand,
    ) -> Result<TurnPlan, DomainError> {
        let gate = self
            .preflight_gate(conn, ctx, scopes, chat, command, None)
            .await?;
        self.preflight_plan(conn, ctx, scopes, chat, command, gate, None)
            .await
    }

    /// The part of the preflight that only reads and decides: catalog model,
    /// content, kill switch, quota (downgrade cascade and daily tool quota),
    /// input length, attachments and vision on the effective model. A retry /
    /// edit runs it before the mutation commits (`replaced` is the turn it
    /// replaces, whose answer is not prior context of the new turn).
    pub(super) async fn preflight_gate<R: DBRunner>(
        &self,
        conn: &R,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &ChatRow,
        command: &SendMessageCommand,
        replaced: Option<Uuid>,
    ) -> Result<PreflightGate, DomainError> {
        let tenant_scope = &scopes.tenant;
        let snapshot = self.policy.current_snapshot(ctx.subject_id()).await?;
        // The chat's model is the selected model of every turn; the body's
        // `model` is ignored. A model that left the catalog is rejected; a
        // disabled one is downgraded by the quota gate.
        if !snapshot
            .model_catalog
            .iter()
            .any(|entry| entry.id == chat.model)
        {
            return Err(DomainError::InvalidModel);
        }
        if command.content.trim().is_empty() {
            return Err(DomainError::EmptyContent);
        }
        if command.web_search && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled {
                feature: "web_search",
            });
        }

        let ready = self
            .load_ready_attachments(conn, ctx, tenant_scope, chat, command)
            .await?;
        let image_count = count_images(&ready);
        let search = SearchFacts {
            documents: conn.ready_documents(tenant_scope, chat.id).await?,
            vector_store: conn.find_vector_store_id(tenant_scope, chat.id).await?,
            code_files: conn
                .ready_code_interpreter_files(tenant_scope, chat.id)
                .await?,
        };
        let prior_context_tokens = match replaced {
            Some(request_id) => {
                conn.latest_assistant_tokens_excluding(tenant_scope, chat.id, request_id)
                    .await?
            }
            None => conn.latest_assistant_tokens(tenant_scope, chat.id).await?,
        }
        .unwrap_or(0);
        let quota = self
            .quota_gate(
                conn,
                ctx,
                &scopes.quota,
                chat,
                &snapshot,
                &QuotaFacts {
                    command,
                    prior_context_tokens,
                    image_count,
                    has_ready_documents: search.available(),
                    has_code_interpreter_files: search.code_available(),
                },
            )
            .await?;

        // From here on the effective model rules.
        check_input_length(&quota.entry, &command.content).map_err(|e| context_error(&e))?;
        let images = self.check_attachments(command, &ready)?;
        self.check_images(&snapshot, &quota.entry, images)?;
        Ok(PreflightGate {
            snapshot,
            quota,
            search,
            image_files: image_files(&ready),
        })
    }

    /// The image guards (DESIGN 3.6): the `disable_images` kill switch, the
    /// `rag.max_images_per_message` limit and the `VISION_INPUT` capability of
    /// the **effective** model (an image is never dropped silently).
    fn check_images(
        &self,
        snapshot: &PolicySnapshot,
        entry: &ModelCatalogEntry,
        images: u32,
    ) -> Result<(), DomainError> {
        if images == 0 {
            return Ok(());
        }
        if snapshot.kill_switches.disable_images {
            return Err(DomainError::FeatureDisabled { feature: "images" });
        }
        if images > self.config.rag.max_images_per_message {
            return Err(DomainError::OutOfRange {
                resource: ResourceKind::Chat,
                field: "image_count",
                reason: "TOO_MANY_IMAGES",
                detail: "the message carries more images than allowed".to_owned(),
            });
        }
        if !entry
            .multimodal_capabilities
            .iter()
            .any(|capability| capability == "VISION_INPUT")
        {
            return Err(DomainError::InvalidInput {
                resource: ResourceKind::Chat,
                field: "content_type",
                reason: "VISION_NOT_SUPPORTED",
                detail: "the model does not accept image inputs".to_owned(),
            });
        }
        Ok(())
    }

    /// The tools, the context assembly and the provider resolution of the
    /// effective model the gate picked. `started` is the request id of the
    /// turn whose own user message is already stored (a retry / edit after
    /// its commit): the history leaves it out.
    #[allow(
        clippy::too_many_arguments,
        reason = "the plan needs the caller, the chat, the command and the gate"
    )]
    pub(super) async fn preflight_plan<R: DBRunner>(
        &self,
        conn: &R,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &ChatRow,
        command: &SendMessageCommand,
        gate: PreflightGate,
        started: Option<Uuid>,
    ) -> Result<TurnPlan, DomainError> {
        let PreflightGate {
            snapshot,
            quota:
                QuotaGate {
                    entry,
                    reserve,
                    tier,
                    downgrade_reason,
                    limits,
                    starts,
                },
            search,
            image_files,
        } = gate;
        let ToolPlan { tools, files } = plan_tools(&snapshot, &entry, search, command);
        let Assembled {
            request,
            summary_applied,
            trigger,
        } = self
            .build_request(
                conn,
                ctx,
                &scopes.tenant,
                chat,
                command,
                &entry,
                tools,
                reserve.max_output_tokens_applied,
                &image_files,
                started,
            )
            .await?;
        Ok(TurnPlan {
            entry,
            request,
            web_search_requested: command.web_search,
            files,
            attachment_ids: command.attachment_ids.clone(),
            policy_version: snapshot.policy_version,
            reserve,
            tier,
            downgrade_reason,
            limits,
            starts,
            summary_applied,
            trigger,
        })
    }

    /// The quota gate (DESIGN 3.6 sequence): the downgrade cascade over the
    /// current-period rows, then the daily web search quota of the effective
    /// model. Tokens are reported before `web_search` when both are exhausted.
    async fn quota_gate<R: DBRunner>(
        &self,
        conn: &R,
        ctx: &SecurityContext,
        quota_scope: &AccessScope,
        chat: &ChatRow,
        snapshot: &PolicySnapshot,
        facts: &QuotaFacts<'_>,
    ) -> Result<QuotaGate, DomainError> {
        let user_id = ctx.subject_id();
        let limits = self
            .policy
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        let starts = PeriodStarts::at(now_utc());
        let rows = conn
            .quota_rows(
                quota_scope,
                QuotaOwner {
                    tenant_id: chat.tenant_id,
                    user_id,
                },
                starts,
            )
            .await?;
        let usage = QuotaUsage::from_rows(&rows);

        let reserve_facts = ReserveFacts {
            message: &facts.command.content,
            prior_context_tokens: facts.prior_context_tokens,
            image_count: facts.image_count,
            has_ready_documents: facts.has_ready_documents,
            web_search_requested: facts.command.web_search,
            has_code_interpreter_files: facts.has_code_interpreter_files,
        };
        let streaming_max = self.config.streaming.max_output_tokens;
        let reserve_of = |entry: &ModelCatalogEntry| {
            estimate_reserve(
                entry,
                &snapshot.kill_switches,
                streaming_max,
                &reserve_facts,
            )
        };
        let decision = resolve_effective_model(&CascadeInput {
            selected_model: &chat.model,
            snapshot,
            usage: &usage,
            limits: &limits,
            reserve_of: &reserve_of,
        })
        .map_err(|_| DomainError::QuotaExceeded { scope: "tokens" })?;
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == decision.model_id)
            .cloned()
            .ok_or_else(|| DomainError::internal("the effective model left the snapshot"))?;

        if facts.command.web_search
            && entry.general_config.tool_support.web_search
            && usage.daily_web_search_calls() >= i64::from(self.config.quota.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded {
                scope: "web_search",
            });
        }
        // The daily code interpreter quota applies only when the tool would be
        // sent: ready spreadsheets, model support, kill switch off.
        if facts.has_code_interpreter_files
            && entry.general_config.tool_support.code_interpreter
            && !snapshot.kill_switches.disable_code_interpreter
            && usage.daily_code_interpreter_calls()
                >= i64::from(self.config.quota.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded {
                scope: "code_interpreter",
            });
        }
        Ok(QuotaGate {
            entry,
            reserve: decision.reserve,
            tier: decision.tier,
            downgrade_reason: decision.downgrade_reason,
            limits,
            starts,
        })
    }

    /// Context assembly and the provider request of the effective model.
    #[allow(
        clippy::too_many_arguments,
        reason = "the request needs the caller, the chat, the command, the model and its tools"
    )]
    async fn build_request<R: DBRunner>(
        &self,
        conn: &R,
        ctx: &SecurityContext,
        tenant_scope: &AccessScope,
        chat: &ChatRow,
        command: &SendMessageCommand,
        entry: &ModelCatalogEntry,
        tools: Vec<ChatTool>,
        max_output_tokens_applied: u32,
        image_files: &[String],
        started: Option<Uuid>,
    ) -> Result<Assembled, DomainError> {
        let tool_set = ToolSet {
            file_search: tools
                .iter()
                .any(|tool| matches!(tool, ChatTool::FileSearch { .. })),
            web_search: tools
                .iter()
                .any(|tool| matches!(tool, ChatTool::WebSearch { .. })),
            code_interpreter: tools
                .iter()
                .any(|tool| matches!(tool, ChatTool::CodeInterpreter { .. })),
        };
        let budget = token_budget(entry, max_output_tokens_applied, tool_set)
            .ok_or_else(|| context_error(&ContextError::BudgetExceeded))?;

        let instructions = self.instructions(entry, tool_set);
        // The chat's summary: its text is part of the context when it fits, its
        // frontier bounds the recent history either way.
        let summary = conn.find_summary(tenant_scope, chat.id).await?;
        let history = self
            .recent_history(
                conn,
                tenant_scope,
                chat,
                started,
                summary.as_ref().map(|row| row.frontier),
            )
            .await?;
        let planned = plan_context_with_summary(
            &ContextInput {
                token_budget: budget,
                budgets: &entry.estimation_budgets,
                system_prompt: &instructions,
                history: &history,
                user_message: &command.content,
                image_count: u32::try_from(image_files.len()).unwrap_or(u32::MAX),
            },
            summary.as_ref().and_then(SummaryRow::context_text),
        )
        .map_err(|e| context_error(&e))?;
        let trigger = SummaryTrigger {
            base: summary.as_ref().map(|row| row.frontier),
            assembled_tokens: planned.assembled_tokens,
            effective_budget: effective_budget(entry, max_output_tokens_applied),
            truncated: planned.context.messages_truncated,
        };
        let summary_item = summary
            .as_ref()
            .filter(|_| planned.summary_kept)
            .and_then(|row| row.context_text().map(|text| (row.tokens(), text)));

        self.llm
            .check_provider(chat.tenant_id, &entry.provider_id)?;

        let mut input: Vec<ChatInputItem> = Vec::new();
        // Delivered as a `user` message (never as part of the system prompt),
        // in front of the recent messages.
        if let Some((_, text)) = summary_item {
            input.push(ChatInputItem {
                role: HistoryRole::User,
                content: summary_message(text),
                images: Vec::new(),
            });
        }
        input.extend(
            planned
                .context
                .history
                .into_iter()
                .map(|message| ChatInputItem {
                    role: message.role,
                    content: message.content,
                    images: Vec::new(),
                }),
        );
        input.push(ChatInputItem {
            role: HistoryRole::User,
            content: command.content.clone(),
            images: image_files.to_vec(),
        });
        Ok(Assembled {
            request: ChatRequest {
                tenant_id: chat.tenant_id,
                user_id: ctx.subject_id(),
                chat_id: chat.id,
                provider_id: entry.provider_id.clone(),
                provider_model_id: entry.provider_model_id.clone(),
                instructions,
                input,
                tools,
                max_output_tokens: max_output_tokens_applied,
                max_tool_calls: entry.max_tool_calls,
                api_params: entry.general_config.api_params.clone(),
                is_summary: false,
            },
            summary_applied: summary_item.map(|(tokens, _)| tokens),
            trigger,
        })
    }

    /// The attachments of the request that are usable (same chat, same
    /// uploader, `ready`, not deleted). Lenient: the strict validation of
    /// [`Self::check_attachments`] runs after the quota gate, but the quota
    /// gate already needs the number of images for the reserve.
    async fn load_ready_attachments<R: DBRunner>(
        &self,
        conn: &R,
        ctx: &SecurityContext,
        tenant_scope: &AccessScope,
        chat: &ChatRow,
        command: &SendMessageCommand,
    ) -> Result<Vec<ReadyAttachment>, DomainError> {
        let ids = &command.attachment_ids;
        if ids.is_empty() || !self.attachment_count_valid(ids) {
            return Ok(Vec::new());
        }
        conn.find_ready_attachments(tenant_scope, chat.id, ctx.subject_id(), ids)
            .await
    }

    /// The ids are unique and within `max_documents_per_chat +
    /// max_images_per_message`.
    fn attachment_count_valid(&self, ids: &[Uuid]) -> bool {
        let rag = &self.config.rag;
        let limit = usize::try_from(
            u64::from(rag.max_documents_per_chat) + u64::from(rag.max_images_per_message),
        )
        .unwrap_or(usize::MAX);
        let unique: HashSet<&Uuid> = ids.iter().collect();
        unique.len() == ids.len() && ids.len() <= limit
    }

    /// Validates `attachment_ids` against the usable attachments found
    /// (unique, within the limits, every id found) and returns the number of
    /// images among them.
    pub(super) fn check_attachments(
        &self,
        command: &SendMessageCommand,
        ready: &[ReadyAttachment],
    ) -> Result<u32, DomainError> {
        let ids = &command.attachment_ids;
        if ids.is_empty() {
            return Ok(0);
        }
        if !self.attachment_count_valid(ids) || ready.len() != ids.len() {
            return Err(DomainError::InvalidAttachment);
        }
        Ok(count_images(ready))
    }

    /// The catalog system prompt followed by the guard of every tool sent.
    fn instructions(&self, entry: &ModelCatalogEntry, tools: ToolSet) -> String {
        let context = &self.config.context;
        let mut parts: Vec<&str> = vec![entry.system_prompt.as_str()];
        if tools.file_search {
            parts.push(context.file_search_guard.as_str());
        }
        if tools.web_search {
            parts.push(context.web_search_guard.as_str());
        }
        parts.retain(|part| !part.is_empty());
        parts.join("\n\n")
    }

    /// The last `context.recent_messages_limit` messages of the chat in
    /// chronological order (DESIGN 4 "Recent messages query"): not compressed,
    /// and, when the chat has a summary, after its frontier.
    async fn recent_history<R: DBRunner>(
        &self,
        conn: &R,
        tenant_scope: &AccessScope,
        chat: &ChatRow,
        started: Option<Uuid>,
        after: Option<SummaryFrontier>,
    ) -> Result<Vec<HistoryMessage>, DomainError> {
        let rows = conn
            .recent_messages(
                tenant_scope,
                chat.id,
                &RecentQuery {
                    limit: u64::from(self.config.context.recent_messages_limit),
                    excluded_request: started,
                    after,
                },
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let role = match row.role.as_str() {
                    "user" => HistoryRole::User,
                    "assistant" => HistoryRole::Assistant,
                    _ => return None,
                };
                Some(HistoryMessage {
                    role,
                    content: row.content,
                })
            })
            .collect())
    }
}
