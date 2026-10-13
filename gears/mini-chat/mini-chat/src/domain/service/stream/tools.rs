//! Tool enablement decision
//! (`cpt-cf-mini-chat-algo-streaming-turns-decide-tools`).

use mini_chat_sdk::WebSearchContextSize;
use toolkit_macros::domain_model;

use super::inputs::TurnInputs;
use super::ports::CitationMap;
use crate::config::ProviderKind;
use crate::domain::service::quota::preflight::PreflightDecision;

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-mcp-absent:p1
/// A tool of the provider request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    /// `file_search` over the chat vector store.
    FileSearch {
        /// The chat vector store id.
        vector_store_id: String,
        /// Catalog `max_num_results`.
        max_num_results: u32,
    },
    /// `web_search`.
    WebSearch {
        /// Catalog `web_search_context_size`.
        search_context_size: WebSearchContextSize,
    },
    /// `code_interpreter` with an auto container.
    CodeInterpreter {
        /// Provider file ids of the ready code-interpreter attachments.
        file_ids: Vec<String>,
    },
    /// The `search_knowledge` function tool.
    SearchKnowledge,
}

impl ToolSpec {
    /// The wire name (`file_search`, `web_search`, `code_interpreter`,
    /// `search_knowledge`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
            Self::SearchKnowledge => "search_knowledge",
        }
    }

    /// `file_search`, `web_search` and `code_interpreter`.
    #[must_use]
    pub const fn is_builtin(&self) -> bool {
        !matches!(self, Self::SearchKnowledge)
    }
}

/// What the knowledge-search provider entry offers.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeProviderInfo {
    /// Adapter kind of `knowledge_search.provider_id`.
    pub kind: ProviderKind,
    /// The entry has an alias for the tenant.
    pub has_alias: bool,
    /// The entry's `api_version`.
    pub api_version: Option<String>,
}

/// The knowledge-search configuration and readiness of one turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeInput {
    /// `knowledge_search.enabled`.
    pub enabled: bool,
    /// The provider entry named by `knowledge_search.provider_id`, when it
    /// resolved.
    pub provider: Option<KnowledgeProviderInfo>,
    /// The knowledge retriever is configured.
    pub retriever_configured: bool,
    /// `knowledge_search.guard`.
    pub guard: String,
    /// `knowledge_search.max_calls_per_message`.
    pub max_calls_per_message: u32,
    /// `knowledge_search.top_k`.
    pub top_k: usize,
    /// `knowledge_search.max_chunk_chars`.
    pub max_chunk_chars: usize,
}

/// The knowledge-search parameters kept for the agentic loop.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeParams {
    /// `knowledge_search.max_calls_per_message`.
    pub max_calls_per_message: u32,
    /// `knowledge_search.top_k`.
    pub top_k: usize,
    /// `knowledge_search.max_chunk_chars`.
    pub max_chunk_chars: usize,
}

/// The guard texts of the context configuration.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardTexts {
    /// `context.file_search_guard`.
    pub file_search: String,
    /// `context.web_search_guard`.
    pub web_search: String,
}

/// Inputs of the decision.
#[derive(Debug, Clone, Copy)]
pub struct ToolsInput<'a> {
    /// The preflight decision (effective entry, tool flags, kill switches).
    pub decision: &'a PreflightDecision,
    /// The turn inputs.
    pub inputs: &'a TurnInputs,
    /// The guard texts.
    pub guards: &'a GuardTexts,
    /// The knowledge-search configuration and readiness.
    pub knowledge: &'a KnowledgeInput,
}

/// The tools of the turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolsDecision {
    /// The tools in order: `file_search`, `web_search`, `code_interpreter`,
    /// `search_knowledge`.
    pub tools: Vec<ToolSpec>,
    /// The guard texts in order `file_search`, `web_search`, `search_knowledge`.
    pub guards: Vec<String>,
    /// `metadata.feature`: `none` or the built-in tools joined by `+`.
    pub feature_label: String,
    /// The citation map, kept only with `file_search`.
    pub citation_map: CitationMap,
    /// The knowledge-search parameters, when the tool was added.
    pub knowledge: Option<KnowledgeParams>,
}

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-tool-enablement:p1
/// Decides the tools of the turn from the effective model's `tool_support`,
/// never from the adapter kind.
#[must_use]
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-no-mcp
pub fn decide_tools(input: &ToolsInput<'_>) -> ToolsDecision {
    let decision = input.decision;
    let entry = &decision.entry;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-tool-basis
    let support = &entry.general_config.tool_support;
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-tool-basis
    let switches = &decision.snapshot.kill_switches;
    let inputs = input.inputs;

    let mut result = ToolsDecision::default();
    let mut file_search = false;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-file-search
    if inputs.has_ready_file_search_documents
        && support.file_search
        && !switches.disable_file_search
        && let Some(vector_store_id) = &inputs.vector_store_id
    {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-file-search
        file_search = true;
        result.tools.push(ToolSpec::FileSearch {
            vector_store_id: vector_store_id.clone(),
            max_num_results: entry.max_num_results,
        });
        result.guards.push(input.guards.file_search.clone());
        result.citation_map.clone_from(&inputs.citation_map);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-file-search
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-file-search
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-web-search
    if decision.web_search_allowed {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-web-search
        result.tools.push(ToolSpec::WebSearch {
            search_context_size: entry.web_search_context_size,
        });
        result.guards.push(input.guards.web_search.clone());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-web-search
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-web-search
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-code-interpreter
    if decision.code_interpreter_allowed {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-code-interpreter
        result.tools.push(ToolSpec::CodeInterpreter {
            file_ids: inputs.code_interpreter_file_ids.clone(),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-code-interpreter
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-code-interpreter
    let knowledge = input.knowledge;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-knowledge
    if knowledge.enabled && !file_search {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-knowledge-ready
        match knowledge_gap(knowledge) {
            None => {
                // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-knowledge
                result.tools.push(ToolSpec::SearchKnowledge);
                result.guards.push(knowledge.guard.clone());
                result.knowledge = Some(KnowledgeParams {
                    max_calls_per_message: knowledge.max_calls_per_message,
                    top_k: knowledge.top_k,
                    max_chunk_chars: knowledge.max_chunk_chars,
                });
                // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-add-knowledge
            }
            Some(missing) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-knowledge-off
                tracing::warn!("knowledge search is off for this turn: missing {missing}");
                // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-knowledge-off
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-knowledge-ready
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-if-knowledge
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-feature-label
    let builtin: Vec<&str> = result
        .tools
        .iter()
        .filter(|tool| tool.is_builtin())
        .map(ToolSpec::name)
        .collect();
    result.feature_label = if builtin.is_empty() {
        "none".to_owned()
    } else {
        builtin.join("+")
    };
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-feature-label
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-return-tools
    result
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-return-tools
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-decide-tools:p1:inst-no-mcp

/// The first missing prerequisite of knowledge search, if any.
fn knowledge_gap(knowledge: &KnowledgeInput) -> Option<&'static str> {
    let Some(provider) = &knowledge.provider else {
        return Some("provider");
    };
    if !matches!(
        provider.kind,
        ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
    ) {
        return Some("provider kind");
    }
    if !provider.has_alias {
        return Some("alias");
    }
    if provider.api_version.as_deref().is_none_or(str::is_empty) {
        return Some("api_version");
    }
    if !knowledge.retriever_configured {
        return Some("retriever");
    }
    None
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
