//! Tool selection for a turn (DESIGN section 4, "File Search Tool
//! Availability", "Code Interpreter Tool Availability", web search).

use crate::config::ContextConfig;
use crate::domain::estimation::ToolGates;
use crate::domain::ports::ToolSpec;

/// Build the tool list for the gates, in the order `file_search`,
/// `web_search`, `code_interpreter`. `file_search` additionally needs the
/// chat's vector store id; `code_interpreter` needs at least one file id.
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref)] // signature fixed by the plan interface
pub fn select_tools(
    gates: &ToolGates,
    vector_store_id: Option<&str>,
    max_num_results: u32,
    web_search_context_size: &str,
    ci_file_ids: &[String],
) -> Vec<ToolSpec> {
    let mut tools = Vec::new();
    if let (true, Some(id)) = (gates.file_search, vector_store_id) {
        tools.push(ToolSpec::FileSearch {
            vector_store_ids: vec![id.to_owned()],
            max_num_results,
        });
    }
    if gates.web_search {
        tools.push(ToolSpec::WebSearch {
            search_context_size: web_search_context_size.to_owned(),
        });
    }
    if gates.code_interpreter && !ci_file_ids.is_empty() {
        tools.push(ToolSpec::CodeInterpreter {
            file_ids: ci_file_ids.to_vec(),
        });
    }
    tools
}

/// Guard texts for the tools actually sent, in tool order: the file search
/// guard when `file_search` is sent, the web search guard when `web_search` is.
#[must_use]
pub fn tool_guards<'a>(tools: &[ToolSpec], cfg: &'a ContextConfig) -> Vec<&'a str> {
    tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::FileSearch { .. } => Some(cfg.file_search_guard.as_str()),
            ToolSpec::WebSearch { .. } => Some(cfg.web_search_guard.as_str()),
            ToolSpec::CodeInterpreter { .. } | ToolSpec::Function { .. } => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tools_tests;
