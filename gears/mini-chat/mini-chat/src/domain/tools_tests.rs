use mini_chat_sdk::KillSwitches;

use super::{ChatToolFacts, KnowledgeParams, guards, select_tools};
use crate::config::{ContextConfig, DEFAULT_WEB_SEARCH_GUARD};
use crate::domain::test_fixtures::standard;
use crate::infra::llm::types::ToolSpec;

fn facts() -> ChatToolFacts {
    ChatToolFacts {
        vector_store_id: Some("vs_1".to_owned()),
        has_ready_docs: true,
        code_interpreter_file_ids: vec!["file_x".to_owned()],
    }
}

fn knowledge() -> KnowledgeParams {
    KnowledgeParams {
        guard: "kb guard".to_owned(),
        tool: ToolSpec::Function {
            name: "search_knowledge".to_owned(),
            description: "d".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        },
    }
}

#[test]
fn file_search_requires_ready_docs_and_support_and_switch() {
    let eff = standard("m");
    let ks = KillSwitches::default();

    let sel = select_tools(&eff, &ks, &facts(), false, None);
    assert!(sel.file_search);
    assert_eq!(
        sel.specs[0],
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_1".to_owned()],
            max_num_results: 5,
        }
    );

    let no_docs = ChatToolFacts {
        has_ready_docs: false,
        ..facts()
    };
    assert!(!select_tools(&eff, &ks, &no_docs, false, None).file_search);

    let no_store = ChatToolFacts {
        vector_store_id: None,
        ..facts()
    };
    assert!(!select_tools(&eff, &ks, &no_store, false, None).file_search);

    let mut unsupported = standard("m");
    unsupported.general_config.tool_support.file_search = false;
    assert!(!select_tools(&unsupported, &ks, &facts(), false, None).file_search);

    let switched = KillSwitches {
        disable_file_search: true,
        ..KillSwitches::default()
    };
    let sel = select_tools(&eff, &switched, &facts(), false, None);
    assert!(!sel.file_search);
    assert!(
        !sel.specs
            .iter()
            .any(|t| matches!(t, ToolSpec::FileSearch { .. }))
    );
}

#[test]
fn web_search_dropped_when_model_lacks_support() {
    let ks = KillSwitches::default();
    let mut eff = standard("m");
    eff.web_search_context_size = "medium".to_owned();
    let sel = select_tools(&eff, &ks, &ChatToolFacts::default(), true, None);
    assert!(sel.web_search);
    assert_eq!(
        sel.specs,
        vec![ToolSpec::WebSearch {
            search_context_size: "medium".to_owned()
        }]
    );

    let mut unsupported = standard("m");
    unsupported.general_config.tool_support.web_search = false;
    let sel = select_tools(&unsupported, &ks, &ChatToolFacts::default(), true, None);
    assert!(!sel.web_search);
    assert!(sel.specs.is_empty());

    // Not requested: not sent.
    assert!(!select_tools(&eff, &ks, &ChatToolFacts::default(), false, None).web_search);

    let switched = KillSwitches {
        disable_web_search: true,
        ..KillSwitches::default()
    };
    assert!(!select_tools(&eff, &switched, &ChatToolFacts::default(), true, None).web_search);
}

#[test]
fn code_interpreter_omitted_when_kill_switch() {
    let eff = standard("m");
    let sel = select_tools(&eff, &KillSwitches::default(), &facts(), false, None);
    assert!(sel.code_interpreter);
    assert!(sel.specs.contains(&ToolSpec::CodeInterpreter {
        file_ids: vec!["file_x".to_owned()]
    }));

    let switched = KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    let sel = select_tools(&eff, &switched, &facts(), false, None);
    assert!(!sel.code_interpreter);
    assert!(
        !sel.specs
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
    );

    // No ready code interpreter files: omitted too.
    let none = ChatToolFacts {
        code_interpreter_file_ids: vec![],
        ..facts()
    };
    assert!(!select_tools(&eff, &KillSwitches::default(), &none, false, None).code_interpreter);
}

#[test]
fn knowledge_off_when_file_search_present() {
    let eff = standard("m");
    let ks = KillSwitches::default();
    let k = knowledge();

    let sel = select_tools(&eff, &ks, &facts(), false, Some(&k));
    assert!(sel.file_search);
    assert!(!sel.knowledge);
    assert!(!sel.specs.contains(&k.tool));

    let no_docs = ChatToolFacts {
        has_ready_docs: false,
        ..facts()
    };
    let sel = select_tools(&eff, &ks, &no_docs, false, Some(&k));
    assert!(sel.knowledge);
    assert_eq!(sel.specs.last(), Some(&k.tool));

    assert!(!select_tools(&eff, &ks, &no_docs, false, None).knowledge);
}

#[test]
fn feature_label_joined() {
    let eff = standard("m");
    let ks = KillSwitches::default();
    let sel = select_tools(&eff, &ks, &facts(), true, None);
    assert_eq!(sel.feature_label, "file_search+web_search+code_interpreter");

    let sel = select_tools(&eff, &ks, &ChatToolFacts::default(), false, None);
    assert_eq!(sel.feature_label, "none");

    // The knowledge function tool is not a built-in feature.
    let sel = select_tools(
        &eff,
        &ks,
        &ChatToolFacts::default(),
        true,
        Some(&knowledge()),
    );
    assert!(sel.knowledge);
    assert_eq!(sel.feature_label, "web_search");
}

#[test]
fn guards_follow_the_selected_tools() {
    let eff = standard("m");
    let ks = KillSwitches::default();
    let cfg = ContextConfig::default();
    let k = knowledge();

    let sel = select_tools(&eff, &ks, &facts(), true, Some(&k));
    assert_eq!(
        guards(&sel, &cfg, Some(&k)),
        vec![
            cfg.file_search_guard.as_str(),
            cfg.web_search_guard.as_str()
        ]
    );

    let no_docs = ChatToolFacts {
        has_ready_docs: false,
        ..facts()
    };
    let sel = select_tools(&eff, &ks, &no_docs, false, Some(&k));
    assert_eq!(guards(&sel, &cfg, Some(&k)), vec!["kb guard"]);

    let sel = select_tools(&eff, &ks, &ChatToolFacts::default(), true, None);
    assert_eq!(guards(&sel, &cfg, None), vec![DEFAULT_WEB_SEARCH_GUARD]);

    let sel = select_tools(&eff, &ks, &ChatToolFacts::default(), false, None);
    assert!(guards(&sel, &cfg, None).is_empty());
}
