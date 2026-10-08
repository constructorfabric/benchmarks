#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{select_tools, tool_guards};
use crate::config::ContextConfig;
use crate::domain::estimation::ToolGates;
use crate::domain::ports::ToolSpec;

fn all() -> ToolGates {
    ToolGates {
        file_search: true,
        web_search: true,
        code_interpreter: true,
    }
}

#[test]
fn select_tools_shapes() {
    let ci = vec!["f1".to_owned()];
    let tools = select_tools(&all(), Some("vs1"), 5, "medium", &ci);
    assert_eq!(
        tools,
        vec![
            ToolSpec::FileSearch {
                vector_store_ids: vec!["vs1".to_owned()],
                max_num_results: 5
            },
            ToolSpec::WebSearch {
                search_context_size: "medium".to_owned()
            },
            ToolSpec::CodeInterpreter {
                file_ids: ci.clone()
            },
        ]
    );
    // file_search needs a vector store; code interpreter needs file ids.
    let tools = select_tools(&all(), None, 5, "low", &[]);
    assert_eq!(
        tools,
        vec![ToolSpec::WebSearch {
            search_context_size: "low".to_owned()
        }]
    );
    // gates off -> nothing
    assert!(select_tools(&ToolGates::default(), Some("vs1"), 5, "low", &ci).is_empty());
}

#[test]
fn tool_guards_follow_sent_tools() {
    let cfg = ContextConfig {
        web_search_guard: "WEB".to_owned(),
        file_search_guard: "FILE".to_owned(),
        ..ContextConfig::default()
    };
    let ci = vec!["f".to_owned()];
    let tools = select_tools(&all(), Some("vs"), 3, "low", &ci);
    assert_eq!(tool_guards(&tools, &cfg), vec!["FILE", "WEB"]);
    let only_ci = select_tools(
        &ToolGates {
            code_interpreter: true,
            ..ToolGates::default()
        },
        None,
        3,
        "low",
        &ci,
    );
    assert!(tool_guards(&only_ci, &cfg).is_empty());
}
