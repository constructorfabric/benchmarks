#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the tool enablement decision
//! (`cpt-cf-mini-chat-algo-streaming-turns-decide-tools`).

use mini_chat_sdk::WebSearchContextSize;
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::test_support::premium_decision;
use crate::domain::service::stream::ports::CitationTarget;
use crate::test_doubles::LogCapture;

fn guards() -> GuardTexts {
    GuardTexts {
        file_search: "FS-GUARD".to_owned(),
        web_search: "WS-GUARD".to_owned(),
    }
}

fn knowledge_ready() -> KnowledgeInput {
    KnowledgeInput {
        enabled: true,
        provider: Some(KnowledgeProviderInfo {
            kind: ProviderKind::OpenaiResponses,
            has_alias: true,
            api_version: Some("2025-01-01-preview".to_owned()),
        }),
        retriever_configured: true,
        guard: "KS-GUARD".to_owned(),
        max_calls_per_message: 3,
        top_k: 5,
        max_chunk_chars: 2000,
    }
}

fn knowledge_off() -> KnowledgeInput {
    KnowledgeInput {
        enabled: false,
        ..knowledge_ready()
    }
}

/// Inputs with a ready document, a vector store and a ready spreadsheet.
fn full_inputs() -> TurnInputs {
    let mut inputs = TurnInputs {
        has_ready_file_search_documents: true,
        has_ready_code_interpreter_files: true,
        code_interpreter_file_ids: vec!["file-sheet1".to_owned()],
        vector_store_id: Some("vs_chat".to_owned()),
        ..TurnInputs::default()
    };
    inputs.citation_map.insert(
        "file-doc1".to_owned(),
        CitationTarget {
            attachment_id: Uuid::from_u128(0x1001),
            filename: "doc1.pdf".to_owned(),
        },
    );
    inputs
}

fn decide(
    decision: &PreflightDecision,
    inputs: &TurnInputs,
    knowledge: &KnowledgeInput,
) -> ToolsDecision {
    decide_tools(&ToolsInput {
        decision,
        inputs,
        guards: &guards(),
        knowledge,
    })
}

fn names(decision: &ToolsDecision) -> Vec<&'static str> {
    decision.tools.iter().map(ToolSpec::name).collect()
}

fn with_web(mut decision: PreflightDecision) -> PreflightDecision {
    decision.web_search_allowed = true;
    decision
}

fn with_code(mut decision: PreflightDecision) -> PreflightDecision {
    decision.code_interpreter_allowed = true;
    decision
}

#[test]
fn nothing_ready_and_nothing_requested_means_no_tools() {
    let result = decide(
        &premium_decision(),
        &TurnInputs::default(),
        &knowledge_off(),
    );
    assert!(result.tools.is_empty());
    assert!(result.guards.is_empty());
    assert_eq!(result.feature_label, "none");
    assert!(result.citation_map.is_empty());
    assert_eq!(result.knowledge, None);
}

// ---- file_search --------------------------------------------------------------------------------

#[test]
fn file_search_is_added_over_the_chat_vector_store() {
    let decision = premium_decision();
    let max_results = decision.entry.max_num_results;
    let result = decide(&decision, &full_inputs(), &knowledge_off());
    assert_eq!(
        result.tools[0],
        ToolSpec::FileSearch {
            vector_store_id: "vs_chat".to_owned(),
            max_num_results: max_results,
        }
    );
    assert_eq!(result.guards, ["FS-GUARD"]);
    assert_eq!(result.feature_label, "file_search");
    assert_eq!(result.citation_map, full_inputs().citation_map);
}

#[test]
fn file_search_needs_a_ready_document() {
    let mut inputs = full_inputs();
    inputs.has_ready_file_search_documents = false;
    let result = decide(&premium_decision(), &inputs, &knowledge_off());
    assert!(!names(&result).contains(&"file_search"));
    assert!(result.citation_map.is_empty(), "the map goes with the tool");
    assert!(!result.guards.contains(&"FS-GUARD".to_owned()));
}

#[test]
fn file_search_needs_model_support() {
    let mut decision = premium_decision();
    decision.entry.general_config.tool_support.file_search = false;
    let result = decide(&decision, &full_inputs(), &knowledge_off());
    assert!(!names(&result).contains(&"file_search"));
    assert!(result.guards.is_empty());
}

#[test]
fn file_search_respects_the_kill_switch() {
    let mut decision = premium_decision();
    decision.snapshot.kill_switches.disable_file_search = true;
    let result = decide(&decision, &full_inputs(), &knowledge_off());
    assert!(!names(&result).contains(&"file_search"));
    assert!(result.guards.is_empty());
}

#[test]
fn file_search_needs_a_vector_store_id() {
    let mut inputs = full_inputs();
    inputs.vector_store_id = None;
    let result = decide(&premium_decision(), &inputs, &knowledge_off());
    assert!(!names(&result).contains(&"file_search"));
    assert!(result.guards.is_empty());
}

// ---- web_search ---------------------------------------------------------------------------------

#[test]
fn web_search_follows_the_decision_and_the_catalog_context_size() {
    let mut decision = with_web(premium_decision());
    decision.entry.web_search_context_size = WebSearchContextSize::High;
    let result = decide(&decision, &TurnInputs::default(), &knowledge_off());
    assert_eq!(
        result.tools,
        vec![ToolSpec::WebSearch {
            search_context_size: WebSearchContextSize::High
        }]
    );
    assert_eq!(result.guards, ["WS-GUARD"]);
    assert_eq!(result.feature_label, "web_search");
}

#[test]
fn a_web_search_request_on_a_model_without_support_adds_neither_tool_nor_guard() {
    // The preflight turns the request flag into `web_search_allowed`; on an
    // unsupported model it is false.
    let mut decision = premium_decision();
    decision.entry.general_config.tool_support.web_search = false;
    decision.web_search_allowed = false;
    let result = decide(&decision, &TurnInputs::default(), &knowledge_off());
    assert!(result.tools.is_empty());
    assert!(result.guards.is_empty());
}

// ---- code_interpreter ---------------------------------------------------------------------------

#[test]
fn code_interpreter_carries_the_ready_file_ids_and_no_guard() {
    let result = decide(
        &with_code(premium_decision()),
        &full_inputs(),
        &knowledge_off(),
    );
    assert!(result.tools.contains(&ToolSpec::CodeInterpreter {
        file_ids: vec!["file-sheet1".to_owned()]
    }));
    assert_eq!(result.guards, ["FS-GUARD"], "code interpreter has no guard");
    assert!(result.feature_label.contains("code_interpreter"));
}

#[test]
fn code_interpreter_is_silently_omitted_when_the_decision_excludes_it() {
    // `code_interpreter_allowed` is false under the kill switch or without
    // model support: the tool is omitted, never an error.
    let result = decide(&premium_decision(), &full_inputs(), &knowledge_off());
    assert!(!names(&result).contains(&"code_interpreter"));
}

// ---- order, label, guards -----------------------------------------------------------------------

#[test]
fn tools_guards_and_label_follow_the_fixed_order() {
    let decision = with_code(with_web(premium_decision()));
    let result = decide(&decision, &full_inputs(), &knowledge_off());
    assert_eq!(
        names(&result),
        ["file_search", "web_search", "code_interpreter"]
    );
    assert_eq!(result.guards, ["FS-GUARD", "WS-GUARD"]);
    assert_eq!(
        result.feature_label,
        "file_search+web_search+code_interpreter"
    );
}

#[test]
fn the_label_names_only_the_built_in_tools_that_are_sent() {
    let result = decide(
        &with_code(with_web(premium_decision())),
        &TurnInputs {
            has_ready_code_interpreter_files: true,
            code_interpreter_file_ids: vec!["f".to_owned()],
            ..TurnInputs::default()
        },
        &knowledge_off(),
    );
    assert_eq!(result.feature_label, "web_search+code_interpreter");
}

// ---- knowledge search ---------------------------------------------------------------------------

#[test]
fn knowledge_search_is_added_last_with_its_guard_and_parameters() {
    let result = decide(
        &with_web(premium_decision()),
        &TurnInputs::default(),
        &knowledge_ready(),
    );
    assert_eq!(names(&result), ["web_search", "search_knowledge"]);
    assert_eq!(result.guards, ["WS-GUARD", "KS-GUARD"]);
    assert_eq!(
        result.knowledge,
        Some(KnowledgeParams {
            max_calls_per_message: 3,
            top_k: 5,
            max_chunk_chars: 2000,
        })
    );
    // It is not a built-in tool and never part of the label.
    assert_eq!(result.feature_label, "web_search");
    assert!(!ToolSpec::SearchKnowledge.is_builtin());
}

#[test]
fn knowledge_search_is_skipped_when_file_search_was_added() {
    let result = decide(&premium_decision(), &full_inputs(), &knowledge_ready());
    assert!(!names(&result).contains(&"search_knowledge"));
    assert!(!result.guards.contains(&"KS-GUARD".to_owned()));
    assert_eq!(result.knowledge, None);
}

#[test]
fn disabled_knowledge_search_adds_nothing_and_does_not_warn() {
    let logs = LogCapture::start();
    let result = decide(
        &premium_decision(),
        &TurnInputs::default(),
        &knowledge_off(),
    );
    assert!(result.tools.is_empty());
    assert!(logs.at(tracing::Level::WARN).is_empty());
}

#[test]
fn anthropic_is_an_accepted_knowledge_provider_kind() {
    let mut knowledge = knowledge_ready();
    knowledge.provider.as_mut().unwrap().kind = ProviderKind::AnthropicMessages;
    let result = decide(&premium_decision(), &TurnInputs::default(), &knowledge);
    assert_eq!(names(&result), ["search_knowledge"]);
}

fn assert_off_with_warning(knowledge: &KnowledgeInput, keyword: &str) {
    let logs = LogCapture::start();
    let result = decide(&premium_decision(), &TurnInputs::default(), knowledge);
    assert!(result.tools.is_empty(), "{keyword}: no tool");
    assert!(result.guards.is_empty(), "{keyword}: no guard");
    assert_eq!(result.knowledge, None, "{keyword}: no parameters");
    let warnings = logs.at(tracing::Level::WARN);
    assert_eq!(warnings.len(), 1, "{keyword}: one warning");
    assert!(
        warnings[0].text().contains(keyword),
        "{keyword}: the warning names the missing item: {}",
        warnings[0].text()
    );
}

#[test]
fn knowledge_search_without_a_resolved_provider_is_off_with_a_warning() {
    let mut knowledge = knowledge_ready();
    knowledge.provider = None;
    assert_off_with_warning(&knowledge, "provider");
}

#[test]
fn knowledge_search_with_an_unsupported_provider_kind_is_off_with_a_warning() {
    for kind in [
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::VllmResponses,
    ] {
        let mut knowledge = knowledge_ready();
        knowledge.provider.as_mut().unwrap().kind = kind;
        assert_off_with_warning(&knowledge, "kind");
    }
}

#[test]
fn knowledge_search_without_an_alias_is_off_with_a_warning() {
    let mut knowledge = knowledge_ready();
    knowledge.provider.as_mut().unwrap().has_alias = false;
    assert_off_with_warning(&knowledge, "alias");
}

#[test]
fn knowledge_search_without_an_api_version_is_off_with_a_warning() {
    for version in [None, Some(String::new())] {
        let mut knowledge = knowledge_ready();
        knowledge.provider.as_mut().unwrap().api_version = version;
        assert_off_with_warning(&knowledge, "api_version");
    }
}

#[test]
fn knowledge_search_without_a_retriever_is_off_with_a_warning() {
    let mut knowledge = knowledge_ready();
    knowledge.retriever_configured = false;
    assert_off_with_warning(&knowledge, "retriever");
}

// ---- MCP ----------------------------------------------------------------------------------------

#[test]
fn mcp_is_never_added_whatever_the_catalog_says() {
    let mut decision = with_code(with_web(premium_decision()));
    decision.entry.general_config.tool_support.mcp = true;
    let result = decide(&decision, &full_inputs(), &knowledge_ready());
    for name in names(&result) {
        assert!(
            [
                "file_search",
                "web_search",
                "code_interpreter",
                "search_knowledge"
            ]
            .contains(&name),
            "unexpected tool {name}"
        );
    }
    assert!(!result.feature_label.contains("mcp"));
}
