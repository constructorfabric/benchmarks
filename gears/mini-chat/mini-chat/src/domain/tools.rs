//! Tool selection for a turn (S§8, D "File Search / Code Interpreter / Web
//! Search Tool Availability", D "Knowledge Search").

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use serde_json::json;

use crate::config::ContextConfig;
use crate::infra::llm::types::{ToolSpec, feature_label};

/// Chat facts the tool gates depend on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatToolFacts {
    /// Provider vector store of the chat, once one exists.
    pub vector_store_id: Option<String>,
    /// The chat has at least one ready, non-deleted document attachment.
    pub has_ready_docs: bool,
    /// Provider file ids of the ready `for_code_interpreter` attachments.
    pub code_interpreter_file_ids: Vec<String>,
}

/// Knowledge search parameters of a request (built by the stream service;
/// `None` when knowledge search is off for it).
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeParams {
    /// `knowledge_search.guard`.
    pub guard: String,
    /// The `search_knowledge` function tool.
    pub tool: ToolSpec,
}

/// Name of the knowledge-search function tool.
pub const SEARCH_KNOWLEDGE: &str = "search_knowledge";

/// The `search_knowledge` function tool: `query` (required) and an
/// optional `top_k` (capped at `knowledge_search.top_k`).
#[must_use]
pub fn search_knowledge_tool() -> ToolSpec {
    ToolSpec::Function {
        name: SEARCH_KNOWLEDGE.to_owned(),
        description: "Search the organization knowledge base and return the most relevant \
                      excerpts."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for"},
                "top_k": {"type": "integer", "minimum": 1, "description": "Maximum number of excerpts"}
            },
            "required": ["query"]
        }),
    }
}

/// Tools of one provider request.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SelectedTools {
    /// In request order: `file_search`, `web_search`, `code_interpreter`,
    /// then `search_knowledge`.
    pub specs: Vec<ToolSpec>,
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
    pub knowledge: bool,
    /// `metadata.feature` of the request.
    pub feature_label: String,
}

/// Tools the effective model `eff` is offered for the request.
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref, reason = "plan signature")]
pub fn select_tools(
    eff: &ModelCatalogEntry,
    ks: &KillSwitches,
    facts: &ChatToolFacts,
    web_search_requested: bool,
    knowledge: Option<&KnowledgeParams>,
) -> SelectedTools {
    let support = &eff.general_config.tool_support;
    let mut specs = Vec::new();

    let vector_store = facts
        .vector_store_id
        .as_ref()
        .filter(|_| facts.has_ready_docs && support.file_search && !ks.disable_file_search);
    let file_search = vector_store.is_some();
    if let Some(vs) = vector_store {
        specs.push(ToolSpec::FileSearch {
            vector_store_ids: vec![vs.clone()],
            max_num_results: eff.max_num_results,
        });
    }

    let web_search = web_search_requested && support.web_search && !ks.disable_web_search;
    if web_search {
        specs.push(ToolSpec::WebSearch {
            search_context_size: eff.web_search_context_size.clone(),
        });
    }

    let code_interpreter = !facts.code_interpreter_file_ids.is_empty()
        && support.code_interpreter
        && !ks.disable_code_interpreter;
    if code_interpreter {
        specs.push(ToolSpec::CodeInterpreter {
            file_ids: facts.code_interpreter_file_ids.clone(),
        });
    }

    // `file_search` wins over knowledge search (D "Knowledge Search").
    let knowledge_tool = knowledge.filter(|_| !file_search);
    if let Some(k) = knowledge_tool {
        specs.push(k.tool.clone());
    }

    let feature_label = feature_label(&specs);
    SelectedTools {
        specs,
        file_search,
        web_search,
        code_interpreter,
        knowledge: knowledge_tool.is_some(),
        feature_label,
    }
}

/// Guard texts of the tools in `sel`, in tool order, for the system prompt.
#[must_use]
pub fn guards<'a>(
    sel: &SelectedTools,
    cfg: &'a ContextConfig,
    knowledge: Option<&'a KnowledgeParams>,
) -> Vec<&'a str> {
    let mut out = Vec::new();
    if sel.file_search {
        out.push(cfg.file_search_guard.as_str());
    }
    if sel.web_search {
        out.push(cfg.web_search_guard.as_str());
    }
    if let Some(k) = knowledge.filter(|_| sel.knowledge) {
        out.push(k.guard.as_str());
    }
    out
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
