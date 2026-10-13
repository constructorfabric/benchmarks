//! Tool set of a provider request (DESIGN "File Search Tool Availability", "Code Interpreter Tool
//! Availability", "Web Search Configuration").

use mini_chat_sdk::ModelCatalogEntry;

use super::quota::ToolGates;
use crate::config::ContextConfig;
use crate::infra::llm::ToolSpec;

/// Tools of one provider request with their guards and surcharge.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSet {
    pub tools: Vec<ToolSpec>,
    /// Guard instructions appended to the system prompt, in tool order.
    pub guards: Vec<String>,
    /// Sum of the effective model's surcharges for the included tools.
    pub surcharge_tokens: i64,
    /// `none`, or the included tools joined with `+` (`file_search`, `web_search`,
    /// `code_interpreter`, in that order).
    pub feature: String,
}

/// Build the tools of the request for the effective `model`. `gates` say which tools the turn
/// wants (after kill switches and model capabilities); `file_search` is only sent when the chat has
/// a vector store.
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref)] // `&ToolGates` is the interface other tasks call
pub fn build_tools(
    gates: &ToolGates,
    model: &ModelCatalogEntry,
    vector_store_id: Option<&str>,
    ci_file_ids: &[String],
    cfg: &ContextConfig,
) -> ToolSet {
    let b = &model.estimation_budgets;
    let mut tools = Vec::new();
    let mut guards = Vec::new();
    let mut names = Vec::new();
    let mut surcharge_tokens = 0_i64;

    if let (true, Some(store)) = (gates.file_search, vector_store_id) {
        tools.push(ToolSpec::FileSearch {
            vector_store_ids: vec![store.to_owned()],
            max_num_results: model.max_num_results,
        });
        guards.push(cfg.file_search_guard.clone());
        names.push("file_search");
        surcharge_tokens = surcharge_tokens.saturating_add(i64::from(b.tool_surcharge_tokens));
    }
    if gates.web_search {
        tools.push(ToolSpec::WebSearch {
            search_context_size: model.web_search_context_size,
        });
        guards.push(cfg.web_search_guard.clone());
        names.push("web_search");
        surcharge_tokens =
            surcharge_tokens.saturating_add(i64::from(b.web_search_surcharge_tokens));
    }
    if gates.code_interpreter {
        tools.push(ToolSpec::CodeInterpreter {
            file_ids: ci_file_ids.to_vec(),
        });
        names.push("code_interpreter");
        surcharge_tokens =
            surcharge_tokens.saturating_add(i64::from(b.code_interpreter_surcharge_tokens));
    }

    let feature = if names.is_empty() {
        "none".to_owned()
    } else {
        names.join("+")
    };
    ToolSet {
        tools,
        guards,
        surcharge_tokens,
        feature,
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::WebSearchContextSize;

    use super::*;
    use crate::config::{DEFAULT_FILE_SEARCH_GUARD, DEFAULT_WEB_SEARCH_GUARD};
    use crate::test_support::catalog::standard_model;

    fn gates(file_search: bool, web_search: bool, code_interpreter: bool) -> ToolGates {
        ToolGates {
            file_search,
            web_search,
            code_interpreter,
        }
    }

    #[test]
    fn tools_and_guards() {
        let model = standard_model("m");
        let cfg = ContextConfig::default();

        // web_search only
        let set = build_tools(&gates(false, true, false), &model, None, &[], &cfg);
        assert_eq!(
            set.tools,
            vec![ToolSpec::WebSearch {
                search_context_size: WebSearchContextSize::Low
            }]
        );
        assert_eq!(set.guards, vec![DEFAULT_WEB_SEARCH_GUARD.to_owned()]);
        assert_eq!(
            set.guards[0],
            "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request."
        );
        assert_eq!(set.feature, "web_search");
        assert_eq!(
            set.surcharge_tokens,
            i64::from(model.estimation_budgets.web_search_surcharge_tokens)
        );

        // file_search without a vector store id: no tool, no guard, no surcharge
        let set = build_tools(&gates(true, false, false), &model, None, &[], &cfg);
        assert!(set.tools.is_empty() && set.guards.is_empty());
        assert_eq!((set.feature.as_str(), set.surcharge_tokens), ("none", 0));

        // file_search + web_search
        let set = build_tools(&gates(true, true, false), &model, Some("vs"), &[], &cfg);
        assert_eq!(
            set.tools,
            vec![
                ToolSpec::FileSearch {
                    vector_store_ids: vec!["vs".to_owned()],
                    max_num_results: model.max_num_results,
                },
                ToolSpec::WebSearch {
                    search_context_size: WebSearchContextSize::Low
                },
            ]
        );
        assert_eq!(
            set.guards,
            vec![
                DEFAULT_FILE_SEARCH_GUARD.to_owned(),
                DEFAULT_WEB_SEARCH_GUARD.to_owned()
            ]
        );
        assert_eq!(set.feature, "file_search+web_search");
        let b = &model.estimation_budgets;
        assert_eq!(
            set.surcharge_tokens,
            i64::from(b.tool_surcharge_tokens + b.web_search_surcharge_tokens)
        );

        // nothing requested
        let set = build_tools(&gates(false, false, false), &model, Some("vs"), &[], &cfg);
        assert!(set.tools.is_empty() && set.guards.is_empty());
        assert_eq!((set.feature.as_str(), set.surcharge_tokens), ("none", 0));
    }

    #[test]
    fn code_interpreter_has_no_guard_and_is_last() {
        let model = standard_model("m");
        let ids = vec!["f1".to_owned(), "f2".to_owned()];
        let set = build_tools(
            &gates(true, true, true),
            &model,
            Some("vs"),
            &ids,
            &ContextConfig::default(),
        );
        assert_eq!(set.feature, "file_search+web_search+code_interpreter");
        assert_eq!(set.tools.len(), 3);
        assert_eq!(set.tools[2], ToolSpec::CodeInterpreter { file_ids: ids });
        assert_eq!(set.guards.len(), 2);
        let b = &model.estimation_budgets;
        assert_eq!(
            set.surcharge_tokens,
            i64::from(
                b.tool_surcharge_tokens
                    + b.web_search_surcharge_tokens
                    + b.code_interpreter_surcharge_tokens
            )
        );
    }
}
