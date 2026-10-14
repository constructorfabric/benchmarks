//! The tool contributions of the feature to the tool assembly of FEATURE 0004
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution`,
//! `-code-interpreter-contribution` and `-knowledge-contribution`) and the source that evaluates
//! them for a turn in that order.
//!
//! The functions read through [`AttachmentFactsStore`] lazily: a gated tool, an empty ready
//! document count or a missing vector store stops before the next read. The tool assembly turns
//! the outputs into `ToolPart`s (`file_search` with the chat store and `max_num_results`,
//! `code_interpreter` with the container file ids, the guards, the `include` entry and the
//! citation file map for the provider task).

use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::{KnowledgeSearchConfig, ProviderKind};
use crate::domain::attachments::facts::AttachmentFactsStore;
use crate::domain::error::DomainError;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::streaming::citations::{FileCitationMap, FileRef};

/// The `include` entry of the `code_interpreter` contribution.
pub const CODE_INTERPRETER_INCLUDE: &str = "code_interpreter_call.outputs";

/// The tool name of the knowledge search function tool.
pub const SEARCH_KNOWLEDGE: &str = "search_knowledge";

/// The `file_search` contribution: the tool over the chat's vector store, its guard and the
/// citation file map (`provider_file_id` to attachment id and filename).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchContribution {
    /// Only the chat's store.
    pub vector_store_ids: Vec<String>,
    /// From the effective model's catalog entry; no attribute filter is sent.
    pub max_num_results: u32,
    /// `context.file_search_guard`.
    pub guard: String,
    pub citation_files: FileCitationMap,
}

/// The `code_interpreter` contribution: the container file ids (`container.type` `auto`) and the
/// `include` entry.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeInterpreterContribution {
    /// The ready code interpreter files, ordered by `created_at` then `id`.
    pub file_ids: Vec<String>,
    /// `code_interpreter_call.outputs`.
    pub include: String,
}

/// The tool-result format of the provider kind that serves the turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultFormat {
    /// `openai_responses`: a `function_call_output` input item.
    FunctionCallOutput,
    /// `anthropic_messages`: a `tool_result` content block.
    ToolResult,
}

/// The `search_knowledge` contribution: the function tool (parameters `query` required and
/// `top_k` optional), its guard `knowledge_search.guard` and the tool-result format; no reserve
/// surcharge.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeContribution {
    pub guard: String,
    pub format: ToolResultFormat,
}

/// The inputs of the file search and code interpreter contributions.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct ChatToolInput<'a> {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    /// The effective model entry (after the downgrade cascade).
    pub entry: &'a ModelCatalogEntry,
    pub kill_switches: &'a KillSwitches,
}

/// The `file_search` contribution. `None` when `disable_file_search` is on or the model's
/// `tool_support.file_search` is false (nothing is read), when the ready document count is 0
/// (`file_search` is never sent before the first document is `ready`; the store is not read),
/// or when the chat has no bound vector store (a warning is logged).
///
/// # Errors
///
/// `Internal` when a read fails.
pub async fn file_search_contribution(
    facts: &dyn AttachmentFactsStore,
    input: &ChatToolInput<'_>,
    guard: &str,
) -> Result<Option<FileSearchContribution>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-gated
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-return-gated
    if input.kill_switches.disable_file_search
        || !input.entry.general_config.tool_support.file_search
    {
        return Ok(None);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-return-gated
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-gated
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-ready-docs
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-no-docs
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-return-no-docs
    // `file_search` is never sent before the first document is `ready`; the vector store is not
    // read before that
    if facts
        .ready_document_count(input.tenant_id, input.chat_id)
        .await?
        == 0
    {
        return Ok(None);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-return-no-docs
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-no-docs
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-ready-docs
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-vector-store
    let Some(store) = facts
        .bound_vector_store_id(input.tenant_id, input.chat_id)
        .await?
    else {
        tracing::warn!(chat_id = %input.chat_id, "a ready document has no bound vector store; file_search is not sent");
        return Ok(None);
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-vector-store
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-citation-map
    let citation_files = facts
        .citation_files(input.tenant_id, input.chat_id)
        .await?
        .into_iter()
        .map(|file| {
            (
                file.provider_file_id,
                FileRef {
                    attachment_id: file.attachment_id,
                    filename: file.filename,
                },
            )
        })
        .collect();
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-citation-map
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-definition
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-guard
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-no-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-return
    // only the chat's store and no attribute filter (`attachment_ids` never scopes retrieval); no
    // per-turn call limit is added
    Ok(Some(FileSearchContribution {
        vector_store_ids: vec![store],
        max_num_results: input.entry.max_num_results,
        guard: guard.to_owned(),
        citation_files,
    }))
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-return
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-no-limit
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-guard
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution:p1:inst-fs-definition
}

/// The `code_interpreter` contribution. `None`, silently, when `disable_code_interpreter` is on or
/// the model's `tool_support.code_interpreter` is false (nothing is read), or when no ready code
/// interpreter file exists.
///
/// # Errors
///
/// `Internal` when the read fails.
pub async fn code_interpreter_contribution(
    facts: &dyn AttachmentFactsStore,
    input: &ChatToolInput<'_>,
) -> Result<Option<CodeInterpreterContribution>, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-gated
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-return-gated
    if input.kill_switches.disable_code_interpreter
        || !input.entry.general_config.tool_support.code_interpreter
    {
        return Ok(None);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-return-gated
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-gated
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-files
    let files = facts
        .ready_code_interpreter_files(input.tenant_id, input.chat_id)
        .await?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-files
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-none
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-return-none
    if files.is_empty() {
        return Ok(None);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-return-none
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-none
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-definition
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-return
    // there is no request flag for code interpreter
    Ok(Some(CodeInterpreterContribution {
        file_ids: files
            .into_iter()
            .map(|file| file.provider_file_id)
            .collect(),
        include: CODE_INTERPRETER_INCLUDE.to_owned(),
    }))
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-return
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-code-interpreter-contribution:p1:inst-ci-definition
}

/// The inputs of the knowledge search contribution.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct KnowledgeInput<'a> {
    pub config: &'a KnowledgeSearchConfig,
    /// Whether the `file_search` contribution applied for the turn (it wins).
    pub file_search_applied: bool,
    pub providers: &'a ProviderRegistry,
    pub tenant_id: Uuid,
    /// Whether the knowledge retriever is configured.
    pub retriever_configured: bool,
}

/// The `search_knowledge` contribution. `None` when `knowledge_search.enabled` is false (the
/// default), when `file_search` applied, or when the provider named by
/// `knowledge_search.provider_id` is not of kind `openai_responses` or `anthropic_messages`, has
/// no upstream alias for the tenant, has an empty `api_version`, or the retriever is not
/// configured (a warning is logged; neither tool nor guard).
#[must_use]
pub fn knowledge_contribution(input: &KnowledgeInput<'_>) -> Option<KnowledgeContribution> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-disabled
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return-disabled
    if !input.config.enabled {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return-disabled
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-disabled
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-exclusive
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return-exclusive
    // `file_search` wins: the two tools are never sent in one request
    if input.file_search_applied {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return-exclusive
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-exclusive
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-unusable
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return-unusable
    let provider = input
        .config
        .provider_id
        .as_deref()
        .and_then(|id| input.providers.resolve(id, input.tenant_id).ok());
    let format = provider.and_then(|provider| {
        let usable = !provider.upstream_alias.is_empty()
            && provider
                .api_version
                .as_deref()
                .is_some_and(|v| !v.is_empty())
            && input.retriever_configured;
        match (usable, provider.kind) {
            (true, ProviderKind::OpenaiResponses) => Some(ToolResultFormat::FunctionCallOutput),
            (true, ProviderKind::AnthropicMessages) => Some(ToolResultFormat::ToolResult),
            _ => None,
        }
    });
    let Some(format) = format else {
        tracing::warn!("knowledge search is enabled but its provider is unusable; no tool is sent");
        return None;
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return-unusable
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-unusable
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-guard
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return
    // the definition itself (`query` required, `top_k` optional) is built by the request builder;
    // no reserve surcharge is added
    Some(KnowledgeContribution {
        guard: input.config.guard.clone(),
        format,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-return
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-guard
}

/// What the three contributions gave for one turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnContributions {
    pub file_search: Option<FileSearchContribution>,
    pub code_interpreter: Option<CodeInterpreterContribution>,
    pub knowledge: Option<KnowledgeContribution>,
}

/// The configuration the contributions read.
#[domain_model]
#[derive(Clone)]
pub struct AttachmentToolSettings {
    /// `context.file_search_guard`.
    pub file_search_guard: String,
    pub knowledge: KnowledgeSearchConfig,
    pub providers: Arc<ProviderRegistry>,
    pub retriever_configured: bool,
}

/// Evaluates the contributions of a turn in the order file search, code interpreter, knowledge
/// search.
#[domain_model]
pub struct AttachmentToolSource {
    facts: Arc<dyn AttachmentFactsStore>,
    settings: AttachmentToolSettings,
}

impl AttachmentToolSource {
    /// Create the source over the fact reads and the settings.
    #[must_use]
    pub fn new(facts: Arc<dyn AttachmentFactsStore>, settings: AttachmentToolSettings) -> Self {
        Self { facts, settings }
    }

    /// The contributions of a turn of the chat in `input`.
    ///
    /// # Errors
    ///
    /// `Internal` when a read fails.
    pub async fn contributions(
        &self,
        input: &ChatToolInput<'_>,
    ) -> Result<TurnContributions, DomainError> {
        let file_search =
            file_search_contribution(&*self.facts, input, &self.settings.file_search_guard).await?;
        let code_interpreter = code_interpreter_contribution(&*self.facts, input).await?;
        let knowledge = knowledge_contribution(&KnowledgeInput {
            config: &self.settings.knowledge,
            file_search_applied: file_search.is_some(),
            providers: &self.settings.providers,
            tenant_id: input.tenant_id,
            retriever_configured: self.settings.retriever_configured,
        });
        Ok(TurnContributions {
            file_search,
            code_interpreter,
            knowledge,
        })
    }
}

#[cfg(test)]
#[path = "contributions_tests.rs"]
mod contributions_tests;
