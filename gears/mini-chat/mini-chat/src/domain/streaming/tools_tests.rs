//! Tool assembly (slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`): the
//! web search gate (catalog `tool_support`, kill switch, request flag), the guard, a stub
//! contribution, ordering and the `feature` metadata value.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-tool-assembly`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, WebSearchContextSize};
use serde_json::json;
use uuid::Uuid;

use super::{ToolContribution, ToolDefinition, ToolKind, ToolPart, ToolTurnFacts, assemble_tools};
use crate::test_support::send::{WEB_GUARD, std_model, with_web_search};

fn facts(web_search_enabled: bool) -> ToolTurnFacts {
    ToolTurnFacts {
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        web_search_enabled,
    }
}

fn supported() -> ModelCatalogEntry {
    with_web_search(
        &std_model(&json!({ "web_search_context_size": "medium" })),
        true,
    )
}

fn unsupported() -> ModelCatalogEntry {
    with_web_search(&std_model(&json!({})), false)
}

/// A contribution that adds a fixed part (or nothing) and records the facts it was asked about.
struct Stub {
    kind: ToolKind,
    part: Option<ToolPart>,
    seen: Mutex<Vec<ToolTurnFacts>>,
}

impl Stub {
    fn new(kind: ToolKind, guard: Option<&str>, include: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            part: Some(ToolPart {
                definition: ToolDefinition {
                    kind,
                    search_context_size: None,
                },
                guard: guard.map(ToOwned::to_owned),
                include: include.map(ToOwned::to_owned),
            }),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn inapplicable(kind: ToolKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            part: None,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl ToolContribution for Stub {
    fn kind(&self) -> ToolKind {
        self.kind
    }

    fn contribute(&self, facts: &ToolTurnFacts) -> Option<ToolPart> {
        self.seen.lock().unwrap().push(facts.clone());
        self.part.clone()
    }
}

fn assemble(
    entry: &ModelCatalogEntry,
    web: bool,
    contributions: &[Arc<dyn ToolContribution>],
) -> super::ToolSet {
    assemble_tools(
        entry,
        &KillSwitches::default(),
        WEB_GUARD,
        &facts(web),
        contributions,
    )
}

fn kinds(set: &super::ToolSet) -> Vec<ToolKind> {
    set.tools.iter().map(|tool| tool.kind).collect()
}

#[test]
fn an_unrequested_turn_has_an_empty_set_and_feature_none() {
    for entry in [supported(), unsupported()] {
        let set = assemble(&entry, false, &[]);
        assert!(set.tools.is_empty());
        assert!(set.guards.is_empty());
        assert!(set.include.is_empty());
        assert_eq!(set.feature, "none");
    }
}

#[test]
fn web_search_is_added_with_the_catalog_context_size_and_its_guard() {
    let set = assemble(&supported(), true, &[]);
    assert_eq!(
        set.tools,
        vec![ToolDefinition {
            kind: ToolKind::WebSearch,
            search_context_size: Some(WebSearchContextSize::Medium),
        }]
    );
    assert_eq!(set.guards, vec![WEB_GUARD.to_owned()]);
    assert_eq!(set.feature, "web_search");
}

#[test]
fn the_context_size_follows_the_catalog_entry() {
    for (raw, expected) in [
        ("low", WebSearchContextSize::Low),
        ("medium", WebSearchContextSize::Medium),
        ("high", WebSearchContextSize::High),
    ] {
        let entry = with_web_search(&std_model(&json!({ "web_search_context_size": raw })), true);
        let set = assemble(&entry, true, &[]);
        assert_eq!(set.tools[0].search_context_size, Some(expected));
    }
}

#[test]
fn a_model_without_web_search_support_proceeds_without_the_tool_and_the_guard() {
    let set = assemble(&unsupported(), true, &[]);
    assert!(set.tools.is_empty());
    assert!(set.guards.is_empty(), "no guard without the tool");
    assert_eq!(set.feature, "none");
}

#[test]
fn the_kill_switch_keeps_web_search_out_of_the_set() {
    let switches = KillSwitches {
        disable_web_search: true,
        ..KillSwitches::default()
    };
    let set = assemble_tools(&supported(), &switches, WEB_GUARD, &facts(true), &[]);
    assert!(set.tools.is_empty());
    assert!(set.guards.is_empty());
}

#[test]
fn contributions_are_asked_with_the_facts_of_the_turn() {
    let stub = Stub::new(ToolKind::FileSearch, Some("FS-GUARD"), None);
    let turn = facts(true);
    let contributions: Vec<Arc<dyn ToolContribution>> = vec![stub.clone()];
    let _set = assemble_tools(
        &supported(),
        &KillSwitches::default(),
        WEB_GUARD,
        &turn,
        &contributions,
    );
    assert_eq!(stub.seen.lock().unwrap().clone(), vec![turn]);
}

#[test]
fn a_contribution_that_does_not_apply_adds_nothing() {
    let stub = Stub::inapplicable(ToolKind::FileSearch);
    let set = assemble(&supported(), false, &[stub]);
    assert!(set.tools.is_empty());
    assert_eq!(set.feature, "none");
}

#[test]
fn tools_are_ordered_file_search_web_search_code_interpreter_whatever_the_registration_order() {
    let contributions: Vec<Arc<dyn ToolContribution>> = vec![
        Stub::new(
            ToolKind::CodeInterpreter,
            None,
            Some("code_interpreter_call.outputs"),
        ),
        Stub::new(ToolKind::FileSearch, Some("FS-GUARD"), None),
    ];
    let set = assemble(&supported(), true, &contributions);
    assert_eq!(
        kinds(&set),
        vec![
            ToolKind::FileSearch,
            ToolKind::WebSearch,
            ToolKind::CodeInterpreter
        ]
    );
    assert_eq!(set.feature, "file_search+web_search+code_interpreter");
}

#[test]
fn guards_follow_the_context_plan_order_and_include_entries_are_collected() {
    let contributions: Vec<Arc<dyn ToolContribution>> = vec![
        Stub::new(ToolKind::SearchKnowledge, Some("KS-GUARD"), None),
        Stub::new(
            ToolKind::CodeInterpreter,
            None,
            Some("code_interpreter_call.outputs"),
        ),
    ];
    let set = assemble(&supported(), true, &contributions);
    assert_eq!(
        set.guards,
        vec![WEB_GUARD.to_owned(), "KS-GUARD".to_owned()],
        "file_search, web_search, search_knowledge"
    );
    assert_eq!(
        set.include,
        vec!["code_interpreter_call.outputs".to_owned()]
    );

    let contributions: Vec<Arc<dyn ToolContribution>> = vec![
        Stub::new(ToolKind::FileSearch, Some("FS-GUARD"), None),
        Stub::new(ToolKind::SearchKnowledge, None, None),
    ];
    let set = assemble(&supported(), true, &contributions);
    assert_eq!(
        set.guards,
        vec!["FS-GUARD".to_owned(), WEB_GUARD.to_owned()]
    );
}

#[test]
fn file_search_and_search_knowledge_are_never_both_in_the_set() {
    let contributions: Vec<Arc<dyn ToolContribution>> = vec![
        Stub::new(ToolKind::SearchKnowledge, Some("KS-GUARD"), None),
        Stub::new(ToolKind::FileSearch, Some("FS-GUARD"), None),
    ];
    let set = assemble(&unsupported(), false, &contributions);
    assert_eq!(kinds(&set), vec![ToolKind::FileSearch]);
    assert!(!set.guards.contains(&"KS-GUARD".to_owned()));
}

#[test]
fn the_feature_value_names_only_the_built_in_tools() {
    let knowledge: Vec<Arc<dyn ToolContribution>> =
        vec![Stub::new(ToolKind::SearchKnowledge, Some("KS-GUARD"), None)];
    let set = assemble(&unsupported(), false, &knowledge);
    assert_eq!(kinds(&set), vec![ToolKind::SearchKnowledge]);
    assert_eq!(
        set.feature, "none",
        "search_knowledge is not a built-in tool"
    );

    let code: Vec<Arc<dyn ToolContribution>> =
        vec![Stub::new(ToolKind::CodeInterpreter, None, None)];
    let set = assemble(&unsupported(), false, &code);
    assert_eq!(set.feature, "code_interpreter");

    let file: Vec<Arc<dyn ToolContribution>> = vec![Stub::new(ToolKind::FileSearch, None, None)];
    let set = assemble(&supported(), true, &file);
    assert_eq!(set.feature, "file_search+web_search");
}

#[test]
fn assembly_is_deterministic() {
    let contributions: Vec<Arc<dyn ToolContribution>> =
        vec![Stub::new(ToolKind::FileSearch, Some("FS-GUARD"), None)];
    let turn = facts(true);
    let run = || {
        assemble_tools(
            &supported(),
            &KillSwitches::default(),
            WEB_GUARD,
            &turn,
            &contributions,
        )
    };
    assert_eq!(run(), run());
}
