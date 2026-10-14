//! The three tool contributions of the feature over a counting fact store and provider
//! registries: the `file_search` gate (kill switch, `tool_support`), the ready document fact, the
//! bound vector store, the definition with `max_num_results` from the catalog, the guard and the
//! citation file map; the `code_interpreter` gate, the ordered file ids and the `include` entry;
//! the `search_knowledge` rules (disabled by default, mutually exclusive with `file_search`,
//! unusable provider) and the evaluation order of the source.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-file-search-contribution`
//! (`inst-fs-*`), `-code-interpreter-contribution` (`inst-ci-*`), `-knowledge-contribution`
//! (`inst-ks-*`), `-ready-document-fact` and `-ready-code-interpreter-fact` (their consumers),
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`
//! (`inst-ask-contributions`); definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-tool-contributions` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-deferred-scope` (no per-turn limit, no
//! attribute filter).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use tracing_test::traced_test;
use uuid::Uuid;

use super::{
    AttachmentToolSettings, AttachmentToolSource, CODE_INTERPRETER_INCLUDE, ChatToolInput,
    CodeInterpreterContribution, FileSearchContribution, KnowledgeContribution, KnowledgeInput,
    SEARCH_KNOWLEDGE, ToolResultFormat, TurnContributions, code_interpreter_contribution,
    file_search_contribution, knowledge_contribution,
};
use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::facts::AttachmentFactsStore;
use crate::domain::error::DomainError;
use crate::domain::streaming::citations::{FileCitationMap, FileRef};
use crate::test_support::tool_fakes::{
    FactRead, FakeFacts, KB_ALIAS, KB_API_VERSION, KB_PROVIDER, kb_registry, knowledge_config,
    model_with_tools, no_switches, ready_file, switches_off,
};
use crate::test_support::{TENANT_A, TENANT_B};

const CHAT: Uuid = Uuid::from_u128(0xC4A7);
const GUARD: &str = "FILE-GUARD: use file_search for documents.";

fn input<'a>(entry: &'a ModelCatalogEntry, switches: &'a KillSwitches) -> ChatToolInput<'a> {
    ChatToolInput {
        tenant_id: TENANT_A,
        chat_id: CHAT,
        entry,
        kill_switches: switches,
    }
}

fn with_documents(documents: u32, store: Option<&str>) -> FakeFacts {
    FakeFacts {
        documents,
        vector_store: store.map(str::to_owned),
        ..FakeFacts::new()
    }
}

async fn file_search(
    facts: &FakeFacts,
    entry: &ModelCatalogEntry,
    switches: &KillSwitches,
) -> Result<Option<FileSearchContribution>, DomainError> {
    file_search_contribution(facts, &input(entry, switches), GUARD).await
}

// ── file search: the gates ───────────────────────────────────────────────────

#[tokio::test]
async fn the_kill_switch_gates_file_search_before_any_read() {
    // inst-fs-gated, inst-fs-return-gated
    let facts = with_documents(3, Some("vs_1"));
    let entry = model_with_tools(5, true, true);
    let result = file_search(&facts, &entry, &switches_off(true, false)).await;
    assert_eq!(result.unwrap(), None);
    assert!(
        facts.reads().is_empty(),
        "no read behind a gate: {:?}",
        facts.reads()
    );
}

#[tokio::test]
async fn a_model_without_file_search_support_gets_no_contribution_and_no_read() {
    // inst-fs-gated: the effective model's `tool_support.file_search` is false
    let facts = with_documents(3, Some("vs_1"));
    let entry = model_with_tools(5, false, true);
    let result = file_search(&facts, &entry, &no_switches()).await;
    assert_eq!(result.unwrap(), None);
    assert!(facts.reads().is_empty());
}

#[tokio::test]
async fn the_code_interpreter_switch_does_not_gate_file_search() {
    let facts = with_documents(1, Some("vs_1"));
    let entry = model_with_tools(5, true, true);
    let result = file_search(&facts, &entry, &switches_off(false, true)).await;
    assert!(result.unwrap().is_some());
}

#[tokio::test]
async fn file_search_is_never_sent_before_the_first_document_is_ready() {
    // inst-fs-ready-docs, inst-fs-no-docs, inst-fs-return-no-docs: the store is not read
    let facts = with_documents(0, Some("vs_1"));
    let entry = model_with_tools(5, true, true);
    let result = file_search(&facts, &entry, &no_switches()).await;
    assert_eq!(result.unwrap(), None);
    assert_eq!(facts.reads(), vec![FactRead::DocumentCount]);
}

#[traced_test]
#[tokio::test]
async fn a_chat_without_a_bound_vector_store_gets_no_contribution_and_a_warning() {
    // inst-fs-vector-store: no row, or a placeholder with a NULL `vector_store_id`
    let facts = with_documents(2, None);
    let entry = model_with_tools(5, true, true);
    let result = file_search(&facts, &entry, &no_switches()).await;
    assert_eq!(result.unwrap(), None);
    assert_eq!(
        facts.reads(),
        vec![FactRead::DocumentCount, FactRead::VectorStore],
        "the citation files are not read without a store"
    );
    logs_assert(|lines: &[&str]| {
        if lines.iter().any(|line| line.contains("WARN")) {
            Ok(())
        } else {
            Err(format!("no WARN line: {lines:#?}"))
        }
    });
}

#[tokio::test]
async fn a_fact_read_error_is_an_error_not_a_silent_skip() {
    let facts = with_documents(2, Some("vs_1"));
    facts.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let entry = model_with_tools(5, true, true);
    assert!(file_search(&facts, &entry, &no_switches()).await.is_err());
}

// ── file search: the contribution ────────────────────────────────────────────

#[tokio::test]
async fn the_definition_holds_only_the_chat_store_and_the_catalog_max_num_results() {
    // inst-fs-definition, inst-fs-guard, inst-fs-return
    let facts = FakeFacts {
        documents: 2,
        vector_store: Some("vs_chat".to_owned()),
        citations: vec![ready_file(1, "file-1", "report.pdf")],
        ..FakeFacts::new()
    };
    for max in [1, 5, 7] {
        let entry = model_with_tools(max, true, true);
        let found = file_search(&facts, &entry, &no_switches())
            .await
            .unwrap()
            .expect("a contribution");
        assert_eq!(found.vector_store_ids, vec!["vs_chat".to_owned()]);
        assert_eq!(
            found.max_num_results, max,
            "from the effective model's entry"
        );
        assert_eq!(found.guard, GUARD);
    }
}

#[tokio::test]
async fn the_reads_are_the_count_then_the_store_then_the_citation_files() {
    // inst-fs-ready-docs, inst-fs-vector-store, inst-fs-citation-map
    let facts = with_documents(1, Some("vs_chat"));
    let entry = model_with_tools(5, true, true);
    file_search(&facts, &entry, &no_switches()).await.unwrap();
    assert_eq!(
        facts.reads(),
        vec![
            FactRead::DocumentCount,
            FactRead::VectorStore,
            FactRead::CitationFiles
        ]
    );
}

#[tokio::test]
async fn the_citation_map_resolves_provider_file_ids_to_attachment_and_filename() {
    // inst-fs-citation-map: built before the provider call
    let facts = FakeFacts {
        documents: 1,
        vector_store: Some("vs_chat".to_owned()),
        citations: vec![
            ready_file(1, "file-1", "report.pdf"),
            ready_file(2, "file-2", "sheet.xlsx"),
            ready_file(3, "file-3", "photo.png"),
        ],
        ..FakeFacts::new()
    };
    let entry = model_with_tools(5, true, true);
    let found = file_search(&facts, &entry, &no_switches())
        .await
        .unwrap()
        .unwrap();
    let expected: FileCitationMap = [
        ("file-1", 1, "report.pdf"),
        ("file-2", 2, "sheet.xlsx"),
        ("file-3", 3, "photo.png"),
    ]
    .into_iter()
    .map(|(file, n, name)| {
        (
            file.to_owned(),
            FileRef {
                attachment_id: Uuid::from_u128(0xF000 + n),
                filename: name.to_owned(),
            },
        )
    })
    .collect();
    assert_eq!(found.citation_files, expected);
    assert!(
        !found.citation_files.contains_key("file-deleted"),
        "only the live files are in the map"
    );
}

#[tokio::test]
async fn file_search_adds_no_per_turn_limit_and_no_attribute_filter() {
    // inst-fs-no-limit, deferred scope: the contribution carries exactly the store ids, the
    // result bound, the guard and the map
    let facts = with_documents(40, Some("vs_chat"));
    let entry = model_with_tools(5, true, true);
    let found = file_search(&facts, &entry, &no_switches())
        .await
        .unwrap()
        .unwrap();
    let FileSearchContribution {
        vector_store_ids,
        max_num_results,
        guard,
        citation_files,
    } = found;
    assert_eq!(vector_store_ids.len(), 1);
    assert_eq!(max_num_results, 5);
    assert!(!guard.is_empty());
    assert!(citation_files.is_empty());
}

// ── code interpreter ─────────────────────────────────────────────────────────

async fn code_interpreter(
    facts: &FakeFacts,
    entry: &ModelCatalogEntry,
    switches: &KillSwitches,
) -> Result<Option<CodeInterpreterContribution>, DomainError> {
    code_interpreter_contribution(facts, &input(entry, switches)).await
}

#[tokio::test]
async fn the_kill_switch_gates_the_code_interpreter_silently_before_any_read() {
    // inst-ci-gated, inst-ci-return-gated
    let facts = FakeFacts {
        ci_files: vec![ready_file(1, "file-x", "data.xlsx")],
        ..FakeFacts::new()
    };
    let entry = model_with_tools(5, true, true);
    let result = code_interpreter(&facts, &entry, &switches_off(false, true)).await;
    assert_eq!(result.unwrap(), None, "silently: no error, no contribution");
    assert!(facts.reads().is_empty());
}

#[tokio::test]
async fn a_model_without_code_interpreter_support_gets_no_contribution_and_no_read() {
    // inst-ci-gated
    let facts = FakeFacts {
        ci_files: vec![ready_file(1, "file-x", "data.xlsx")],
        ..FakeFacts::new()
    };
    let entry = model_with_tools(5, true, false);
    let result = code_interpreter(&facts, &entry, &no_switches()).await;
    assert_eq!(result.unwrap(), None);
    assert!(facts.reads().is_empty());
}

#[tokio::test]
async fn the_file_search_switch_does_not_gate_the_code_interpreter() {
    let facts = FakeFacts {
        ci_files: vec![ready_file(1, "file-x", "data.xlsx")],
        ..FakeFacts::new()
    };
    let entry = model_with_tools(5, true, true);
    let result = code_interpreter(&facts, &entry, &switches_off(true, false)).await;
    assert!(result.unwrap().is_some());
}

#[tokio::test]
async fn no_ready_file_means_no_code_interpreter_contribution() {
    // inst-ci-files, inst-ci-none, inst-ci-return-none
    let facts = FakeFacts::new();
    let entry = model_with_tools(5, true, true);
    let result = code_interpreter(&facts, &entry, &no_switches()).await;
    assert_eq!(result.unwrap(), None);
    assert_eq!(facts.reads(), vec![FactRead::CodeInterpreterFiles]);
}

#[tokio::test]
async fn the_contribution_lists_the_file_ids_in_order_with_the_include_entry() {
    // inst-ci-definition, inst-ci-return: `container` `{type: auto, file_ids}` and the
    // `code_interpreter_call.outputs` include
    let facts = FakeFacts {
        ci_files: vec![
            ready_file(1, "file-b", "b.xlsx"),
            ready_file(2, "file-a", "a.csv"),
        ],
        ..FakeFacts::new()
    };
    let entry = model_with_tools(5, true, true);
    let found = code_interpreter(&facts, &entry, &no_switches())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        found.file_ids,
        vec!["file-b".to_owned(), "file-a".to_owned()]
    );
    assert_eq!(found.include, "code_interpreter_call.outputs");
    assert_eq!(found.include, CODE_INTERPRETER_INCLUDE);
}

#[tokio::test]
async fn a_code_interpreter_read_error_is_an_error() {
    let facts = FakeFacts::new();
    facts.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let entry = model_with_tools(5, true, true);
    assert!(
        code_interpreter(&facts, &entry, &no_switches())
            .await
            .is_err()
    );
}

// ── knowledge search ─────────────────────────────────────────────────────────

fn knowledge(
    config: &KnowledgeSearchConfig,
    registry: &crate::domain::provider_registry::ProviderRegistry,
    file_search_applied: bool,
    retriever_configured: bool,
    tenant_id: Uuid,
) -> Option<KnowledgeContribution> {
    knowledge_contribution(&KnowledgeInput {
        config,
        file_search_applied,
        providers: registry,
        tenant_id,
        retriever_configured,
    })
}

#[test]
fn knowledge_search_is_disabled_by_default() {
    // inst-ks-disabled, inst-ks-return-disabled
    let registry = kb_registry("openai_responses", Some(KB_API_VERSION));
    let config = KnowledgeSearchConfig {
        vector_store_id: Some("vs_kb".to_owned()),
        provider_id: Some(KB_PROVIDER.to_owned()),
        ..KnowledgeSearchConfig::default()
    };
    assert!(!config.enabled);
    assert_eq!(knowledge(&config, &registry, false, true, TENANT_A), None);
}

#[test]
fn an_enabled_usable_knowledge_search_contributes_with_its_guard() {
    // inst-ks-definition, inst-ks-guard, inst-ks-return
    let registry = kb_registry("openai_responses", Some(KB_API_VERSION));
    let config = knowledge_config();
    let found = knowledge(&config, &registry, false, true, TENANT_A).expect("a contribution");
    assert_eq!(found.guard, config.guard);
    assert_eq!(found.format, ToolResultFormat::FunctionCallOutput);
    assert_eq!(SEARCH_KNOWLEDGE, "search_knowledge");
}

#[test]
fn the_tool_result_format_follows_the_provider_kind() {
    // inst-ks-return: the tool-result format selected by the provider kind
    let config = knowledge_config();
    for (kind, format) in [
        ("openai_responses", ToolResultFormat::FunctionCallOutput),
        ("anthropic_messages", ToolResultFormat::ToolResult),
    ] {
        let registry = kb_registry(kind, Some(KB_API_VERSION));
        let found = knowledge(&config, &registry, false, true, TENANT_A)
            .unwrap_or_else(|| panic!("{kind} must be usable"));
        assert_eq!(found.format, format, "{kind}");
    }
}

#[test]
fn file_search_wins_and_the_two_tools_are_never_sent_together() {
    // inst-ks-exclusive, inst-ks-return-exclusive
    let registry = kb_registry("openai_responses", Some(KB_API_VERSION));
    assert_eq!(
        knowledge(&knowledge_config(), &registry, true, true, TENANT_A),
        None
    );
}

#[test]
#[traced_test]
fn an_unusable_knowledge_provider_gives_no_tool_and_no_guard_with_a_warning() {
    // inst-ks-unusable, inst-ks-return-unusable
    let config = knowledge_config();
    let cases = [
        // not of kind `openai_responses` or `anthropic_messages`
        kb_registry("openai_chat_completions", Some(KB_API_VERSION)),
        kb_registry("vllm_responses", Some(KB_API_VERSION)),
        // an empty `api_version`
        kb_registry("openai_responses", None),
        kb_registry("openai_responses", Some("")),
    ];
    for registry in &cases {
        assert_eq!(knowledge(&config, registry, false, true, TENANT_A), None);
    }
    // the provider named by `provider_id` is not configured
    let ghost = KnowledgeSearchConfig {
        provider_id: Some("ghost".to_owned()),
        ..knowledge_config()
    };
    let registry = kb_registry("openai_responses", Some(KB_API_VERSION));
    assert_eq!(knowledge(&ghost, &registry, false, true, TENANT_A), None);
    // no `provider_id` at all
    let anonymous = KnowledgeSearchConfig {
        provider_id: None,
        ..knowledge_config()
    };
    assert_eq!(
        knowledge(&anonymous, &registry, false, true, TENANT_A),
        None
    );
    // the knowledge retriever is not configured
    assert_eq!(knowledge(&config, &registry, false, false, TENANT_A), None);
    logs_assert(|lines: &[&str]| {
        if lines.iter().any(|line| line.contains("WARN")) {
            Ok(())
        } else {
            Err(format!("no WARN line: {lines:#?}"))
        }
    });
}

#[test]
fn the_provider_is_resolved_for_the_callers_tenant() {
    // inst-ks-unusable: "no upstream alias for the tenant" resolves through the tenant overrides
    let registry = kb_registry("openai_responses", Some(KB_API_VERSION));
    let config = knowledge_config();
    for tenant in [TENANT_A, TENANT_B] {
        assert!(knowledge(&config, &registry, false, true, tenant).is_some());
    }
    assert!(!KB_ALIAS.is_empty());
}

// ── the source ───────────────────────────────────────────────────────────────

fn source(
    facts: FakeFacts,
    knowledge: KnowledgeSearchConfig,
) -> (AttachmentToolSource, Arc<FakeFacts>) {
    let facts = Arc::new(facts);
    let source = AttachmentToolSource::new(
        Arc::clone(&facts) as Arc<dyn AttachmentFactsStore>,
        AttachmentToolSettings {
            file_search_guard: GUARD.to_owned(),
            knowledge,
            providers: kb_registry("openai_responses", Some(KB_API_VERSION)),
            retriever_configured: true,
        },
    );
    (source, facts)
}

#[tokio::test]
async fn the_source_evaluates_file_search_then_code_interpreter_then_knowledge() {
    // inst-ask-contributions: in that order of evaluation
    let (source, facts) = source(
        FakeFacts {
            documents: 1,
            vector_store: Some("vs_chat".to_owned()),
            ci_files: vec![ready_file(1, "file-x", "data.xlsx")],
            ..FakeFacts::new()
        },
        knowledge_config(),
    );
    let entry = model_with_tools(5, true, true);
    let switches = no_switches();
    let all = source
        .contributions(&input(&entry, &switches))
        .await
        .unwrap();
    let reads = facts.reads();
    let position = |read: FactRead| reads.iter().position(|r| *r == read).unwrap();
    assert!(position(FactRead::DocumentCount) < position(FactRead::CodeInterpreterFiles));
    assert!(all.file_search.is_some());
    assert!(all.code_interpreter.is_some());
    assert_eq!(
        all.knowledge, None,
        "file_search wins over search_knowledge"
    );
}

#[tokio::test]
async fn knowledge_search_is_contributed_only_when_file_search_is_absent() {
    // inst-ks-exclusive through the source: an XLSX-only chat has no file_search
    let (source, _facts) = source(
        FakeFacts {
            documents: 0,
            ci_files: vec![ready_file(1, "file-x", "data.xlsx")],
            ..FakeFacts::new()
        },
        knowledge_config(),
    );
    let entry = model_with_tools(5, true, true);
    let switches = no_switches();
    let all = source
        .contributions(&input(&entry, &switches))
        .await
        .unwrap();
    assert_eq!(all.file_search, None);
    assert!(all.code_interpreter.is_some());
    assert!(all.knowledge.is_some());
}

#[tokio::test]
async fn the_kill_switch_of_file_search_lets_knowledge_search_through() {
    // `file_search` did not apply for the turn, so `search_knowledge` may
    let (source, _facts) = source(
        FakeFacts {
            documents: 3,
            vector_store: Some("vs_chat".to_owned()),
            ..FakeFacts::new()
        },
        knowledge_config(),
    );
    let entry = model_with_tools(5, true, true);
    let switches = switches_off(true, false);
    let all = source
        .contributions(&input(&entry, &switches))
        .await
        .unwrap();
    assert_eq!(all.file_search, None);
    assert!(all.knowledge.is_some());
}

#[tokio::test]
async fn a_chat_without_attachments_and_the_default_configuration_contributes_nothing() {
    let (source, _facts) = source(FakeFacts::new(), KnowledgeSearchConfig::default());
    let entry = model_with_tools(5, true, true);
    let switches = no_switches();
    let all = source
        .contributions(&input(&entry, &switches))
        .await
        .unwrap();
    assert_eq!(all, TurnContributions::default());
}

#[tokio::test]
async fn a_read_error_of_a_contribution_fails_the_source() {
    let (source, facts) = source(FakeFacts::new(), KnowledgeSearchConfig::default());
    facts.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let entry = model_with_tools(5, true, true);
    let switches = no_switches();
    assert!(
        source
            .contributions(&input(&entry, &switches))
            .await
            .is_err()
    );
}
