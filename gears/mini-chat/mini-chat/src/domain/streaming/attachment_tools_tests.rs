//! The attachment tool contributions and the shared fact predicates seen through the send setup
//! over in-memory `SQLite`: the send preflight reads the ready document fact and the ready code
//! interpreter fact, so an XLSX-only chat reserves no `file_search` surcharge and sends no
//! `file_search` tool while a chat with a ready PDF does; the tool set of the plan and the provider
//! request carry `file_search` over the chat vector store with `max_num_results` from the catalog
//! entry and the guard, `code_interpreter` with the ready file ids and the `include` entry, and
//! `search_knowledge` only when enabled, usable and `file_search` is absent.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files` (`inst-ask-facts`,
//! `inst-ask-contributions`, `inst-ask-request`), algorithms `-ready-document-fact`,
//! `-ready-code-interpreter-fact`, `-file-search-contribution`, `-code-interpreter-contribution`
//! and `-knowledge-contribution`; cross-feature touchpoint with
//! `cpt-cf-mini-chat-algo-message-streaming-send-preflight` (`inst-pre-read-attachments`), the
//! quota estimate (`inst-est-surcharges`) and `inst-toolq-ci-applies`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-tool-contributions` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-deferred-scope` (no attribute filter, no
//! Anthropic document search).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::KillSwitches;
use serde_json::{Value, json};

use super::provider_request::build_provider_request;
use super::{ProviderCallPlan, ToolKind};
use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::contributions::AttachmentToolSettings;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::seed_vector_store;
use crate::test_support::chats::at;
use crate::test_support::send::{
    AttachmentSeed, FILE_GUARD, SendRig, SendRigOptions, catalog_of, multi_catalog,
    multi_providers, prem_model, rig_providers, seed_att, send_rig_with, std_model, t,
};
use crate::test_support::tool_fakes::{knowledge_config, tool_settings};

const CI_INCLUDE: &str = "code_interpreter_call.outputs";
const STORE: &str = "vs_chat_1";

fn settings(knowledge: KnowledgeSearchConfig) -> AttachmentToolSettings {
    tool_settings(knowledge)
}

async fn rig() -> SendRig {
    send_rig_with(
        SendRigOptions::new().with_attachment_tools(settings(KnowledgeSearchConfig::default())),
    )
    .await
}

async fn rig_with_catalog(snapshot: mini_chat_sdk::PolicySnapshot) -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_snapshot(snapshot)
            .with_attachment_tools(settings(KnowledgeSearchConfig::default())),
    )
    .await
}

async fn bound_chat(rig: &SendRig) -> ChatRecord {
    let chat = rig.std_chat().await;
    seed_vector_store(&rig.quota.db, &chat, Some(STORE), "azure_openai", at(0)).await;
    chat
}

async fn plan(rig: &SendRig, chat: &ChatRecord) -> ProviderCallPlan {
    rig.plan_for(chat, "hello").await
}

fn kinds(plan: &ProviderCallPlan) -> Vec<ToolKind> {
    plan.tools.tools.iter().map(|tool| tool.kind).collect()
}

fn body_of(plan: &ProviderCallPlan) -> Value {
    build_provider_request(plan).unwrap().body
}

fn tool_of<'a>(body: &'a Value, kind: &str) -> Option<&'a Value> {
    body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["type"] == kind))
}

/// A named chat composition with its expected `(file_search, code_interpreter)` tools.
type Case = (&'static str, Vec<AttachmentSeed>, bool, bool);

fn pdf() -> AttachmentSeed {
    AttachmentSeed::ready("document")
}

fn xlsx() -> AttachmentSeed {
    AttachmentSeed::ready("document")
        .code_interpreter()
        .no_file_search()
}

/// The reserve of a chat with the given attachments, seeded into a chat with a bound store.
async fn reserve_of(rig: &SendRig, seeds: Vec<AttachmentSeed>) -> i64 {
    let chat = bound_chat(rig).await;
    for seed in seeds {
        seed_att(&rig.quota.db, &chat, seed).await;
    }
    plan(rig, &chat).await.decision.reserve_tokens
}

// ── the quota estimate follows the shared predicates ─────────────────────────

#[tokio::test]
async fn an_xlsx_only_chat_reserves_no_file_search_surcharge() {
    // inst-ask-facts: kind `document`, `for_file_search` false, `for_code_interpreter` true; the
    // estimate adds the code interpreter surcharge (1000) and never the file search one (500)
    let rig = rig().await;
    let baseline = reserve_of(&rig, vec![]).await;
    let xlsx_only = reserve_of(&rig, vec![xlsx()]).await;
    assert_eq!(
        xlsx_only - baseline,
        1000,
        "the code interpreter surcharge only"
    );
}

#[tokio::test]
async fn a_chat_with_a_ready_pdf_reserves_the_file_search_surcharge() {
    let rig = rig().await;
    let baseline = reserve_of(&rig, vec![]).await;
    let with_pdf = reserve_of(&rig, vec![pdf()]).await;
    assert_eq!(with_pdf - baseline, 500, "the file search surcharge only");
}

#[tokio::test]
async fn a_chat_with_a_pdf_and_an_xlsx_reserves_both_surcharges_once() {
    let rig = rig().await;
    let baseline = reserve_of(&rig, vec![]).await;
    let both = reserve_of(&rig, vec![pdf(), pdf(), xlsx(), xlsx()]).await;
    assert_eq!(
        both - baseline,
        1500,
        "each surcharge once, never per document"
    );
}

#[tokio::test]
async fn images_and_unready_or_deleted_documents_reserve_nothing() {
    let rig = rig().await;
    let baseline = reserve_of(&rig, vec![]).await;
    for (name, seeds) in [
        ("image", vec![AttachmentSeed::ready("image")]),
        ("uploaded pdf", vec![pdf().status("uploaded")]),
        ("failed pdf", vec![pdf().status("failed")]),
        ("deleted pdf", vec![pdf().deleted(t(1))]),
        ("deleted xlsx", vec![xlsx().deleted(t(1))]),
        (
            "xlsx without a provider file",
            vec![xlsx().no_provider_file()],
        ),
    ] {
        assert_eq!(reserve_of(&rig, seeds).await, baseline, "{name}");
    }
}

// ── the surcharge and the tool appear together or not at all ─────────────────

#[tokio::test]
async fn the_file_search_surcharge_and_the_tool_always_appear_together() {
    // inst-rdf-consumers: every consumer is served from the same predicate
    let rig = rig().await;
    let baseline = reserve_of(&rig, vec![]).await;
    let cases: Vec<Case> = vec![
        ("empty", vec![], false, false),
        ("pdf", vec![pdf()], true, false),
        ("xlsx", vec![xlsx()], false, true),
        ("pdf+xlsx", vec![pdf(), xlsx()], true, true),
        ("image", vec![AttachmentSeed::ready("image")], false, false),
        (
            "xlsx+image",
            vec![xlsx(), AttachmentSeed::ready("image")],
            false,
            true,
        ),
        ("deleted pdf", vec![pdf().deleted(t(1))], false, false),
        ("uploaded pdf", vec![pdf().status("uploaded")], false, false),
        (
            "pdf routed to both",
            vec![pdf().code_interpreter()],
            true,
            true,
        ),
    ];
    for (name, seeds, file_search, code_interpreter) in cases {
        let chat = bound_chat(&rig).await;
        for seed in seeds {
            seed_att(&rig.quota.db, &chat, seed).await;
        }
        let plan = plan(&rig, &chat).await;
        let tools = kinds(&plan);
        assert_eq!(
            tools.contains(&ToolKind::FileSearch),
            file_search,
            "{name}: {tools:?}"
        );
        assert_eq!(
            tools.contains(&ToolKind::CodeInterpreter),
            code_interpreter,
            "{name}: {tools:?}"
        );
        let expected =
            baseline + if file_search { 500 } else { 0 } + if code_interpreter { 1000 } else { 0 };
        assert_eq!(
            plan.decision.reserve_tokens, expected,
            "{name}: surcharge and tool differ"
        );
    }
}

// ── the tool set of the plan ─────────────────────────────────────────────────

#[tokio::test]
async fn an_xlsx_only_chat_sends_no_file_search_tool() {
    // inst-ask-facts, inst-fs-no-docs
    let rig = rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, xlsx()).await;
    let plan = plan(&rig, &chat).await;
    assert_eq!(kinds(&plan), vec![ToolKind::CodeInterpreter]);
    assert!(!plan.tools.guards.iter().any(|guard| guard == FILE_GUARD));
    let body = body_of(&plan);
    assert!(tool_of(&body, "file_search").is_none(), "{body}");
    assert!(tool_of(&body, "code_interpreter").is_some(), "{body}");
}

#[tokio::test]
async fn a_chat_with_a_ready_pdf_sends_file_search_with_its_guard() {
    // inst-fs-return, inst-fs-guard
    let rig = rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    let plan = plan(&rig, &chat).await;
    assert_eq!(kinds(&plan), vec![ToolKind::FileSearch]);
    assert_eq!(plan.tools.guards, vec![FILE_GUARD.to_owned()]);
    assert_eq!(plan.tools.feature, "file_search");
    assert!(plan.tools.include.is_empty());
    assert!(
        body_of(&plan)["instructions"]
            .as_str()
            .unwrap()
            .ends_with(FILE_GUARD),
        "the guard ends the instructions"
    );
}

#[tokio::test]
async fn a_chat_with_both_kinds_sends_both_tools_in_order() {
    let rig = rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    seed_att(&rig.quota.db, &chat, xlsx()).await;
    let plan = plan(&rig, &chat).await;
    assert_eq!(
        kinds(&plan),
        vec![ToolKind::FileSearch, ToolKind::CodeInterpreter]
    );
    assert_eq!(plan.tools.feature, "file_search+code_interpreter");
    assert_eq!(plan.tools.include, vec![CI_INCLUDE.to_owned()]);
}

#[tokio::test]
async fn file_search_is_never_sent_before_the_first_document_is_ready() {
    // inst-fs-no-docs: documents that are not `ready` do not count
    let rig = rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf().status("uploaded")).await;
    seed_att(&rig.quota.db, &chat, pdf().status("pending")).await;
    assert!(kinds(&plan(&rig, &chat).await).is_empty());
}

#[tokio::test]
async fn a_chat_without_a_bound_vector_store_sends_no_file_search() {
    // inst-fs-vector-store: no row, or a placeholder with a NULL `vector_store_id`
    let rig = rig().await;
    let no_row = rig.std_chat().await;
    seed_att(&rig.quota.db, &no_row, pdf()).await;
    assert!(!kinds(&plan(&rig, &no_row).await).contains(&ToolKind::FileSearch));
    let placeholder = rig.std_chat().await;
    seed_vector_store(&rig.quota.db, &placeholder, None, "azure_openai", at(0)).await;
    seed_att(&rig.quota.db, &placeholder, pdf()).await;
    assert!(!kinds(&plan(&rig, &placeholder).await).contains(&ToolKind::FileSearch));
}

#[tokio::test]
async fn a_deleted_document_is_excluded_from_the_tool_inclusion_check() {
    // inst-dtx-exclusion: only `ready` and `deleted_at IS NULL` documents count
    let rig = rig().await;
    let before = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &before, pdf().deleted(t(1))).await;
    assert!(kinds(&plan(&rig, &before).await).is_empty());
    let after = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &after, pdf().deleted(t(1))).await;
    seed_att(&rig.quota.db, &after, pdf()).await;
    assert_eq!(kinds(&plan(&rig, &after).await), vec![ToolKind::FileSearch]);
}

#[tokio::test]
async fn the_kill_switches_and_tool_support_gate_the_two_tools_independently() {
    // inst-fs-gated, inst-ci-gated
    let mut snapshot = catalog_of(vec![prem_model(&json!({})), std_model(&json!({}))]);
    snapshot.kill_switches = KillSwitches {
        disable_file_search: true,
        ..KillSwitches::default()
    };
    let rig = rig_with_catalog(snapshot).await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    seed_att(&rig.quota.db, &chat, xlsx()).await;
    assert_eq!(
        kinds(&plan(&rig, &chat).await),
        vec![ToolKind::CodeInterpreter]
    );

    let mut snapshot = catalog_of(vec![prem_model(&json!({})), std_model(&json!({}))]);
    snapshot.kill_switches = KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    let rig = rig_with_catalog(snapshot).await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    seed_att(&rig.quota.db, &chat, xlsx()).await;
    let tools = plan(&rig, &chat).await;
    assert_eq!(kinds(&tools), vec![ToolKind::FileSearch]);
    assert!(
        tools.tools.include.is_empty(),
        "no include without the code interpreter"
    );
}

#[tokio::test]
async fn a_model_without_file_search_support_gets_no_file_search_and_no_surcharge() {
    let mut entry = serde_json::to_value(std_model(&json!({}))).unwrap();
    entry["general_config"]["tool_support"]["file_search"] = json!(false);
    let entry = serde_json::from_value(entry).unwrap();
    let rig = rig_with_catalog(catalog_of(vec![prem_model(&json!({})), entry])).await;
    let baseline = reserve_of(&rig, vec![]).await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    let plan = plan(&rig, &chat).await;
    assert!(kinds(&plan).is_empty());
    assert_eq!(plan.decision.reserve_tokens, baseline);
}

// ── the provider request ─────────────────────────────────────────────────────

#[tokio::test]
async fn the_file_search_tool_covers_the_whole_chat_store_without_a_filter() {
    // inst-fs-definition, inst-ask-request, deferred scope: no attribute filter, no per-turn limit,
    // `attachment_ids` never scopes retrieval
    let rig = rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    let body = body_of(&plan(&rig, &chat).await);
    let tool = tool_of(&body, "file_search").expect("file_search");
    assert_eq!(
        tool,
        &json!({ "type": "file_search", "vector_store_ids": [STORE], "max_num_results": 5 })
    );
}

#[tokio::test]
async fn max_num_results_follows_the_effective_models_catalog_entry() {
    // inst-fs-definition, inst-cfg-policy
    let rig = rig_with_catalog(catalog_of(vec![
        prem_model(&json!({})),
        std_model(&json!({ "max_num_results": 9 })),
    ]))
    .await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    let body = body_of(&plan(&rig, &chat).await);
    assert_eq!(tool_of(&body, "file_search").unwrap()["max_num_results"], 9);
}

#[tokio::test]
async fn the_code_interpreter_tool_lists_the_ready_files_in_creation_order() {
    // inst-ci-definition, inst-rci-select
    let rig = rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(
        &rig.quota.db,
        &chat,
        xlsx().file_id("file-late").created(t(30)),
    )
    .await;
    seed_att(
        &rig.quota.db,
        &chat,
        xlsx().file_id("file-early").created(t(10)),
    )
    .await;
    seed_att(
        &rig.quota.db,
        &chat,
        xlsx().file_id("file-mid").created(t(20)),
    )
    .await;
    seed_att(
        &rig.quota.db,
        &chat,
        xlsx().file_id("file-deleted").deleted(t(1)),
    )
    .await;
    let body = body_of(&plan(&rig, &chat).await);
    assert_eq!(
        tool_of(&body, "code_interpreter").unwrap(),
        &json!({
            "type": "code_interpreter",
            "container": { "type": "auto", "file_ids": ["file-early", "file-mid", "file-late"] }
        })
    );
    assert_eq!(body["include"], json!([CI_INCLUDE]));
}

#[tokio::test]
async fn the_include_entry_appears_once_and_only_with_the_code_interpreter() {
    let rig = rig().await;
    let only_pdf = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &only_pdf, pdf()).await;
    assert!(
        body_of(&plan(&rig, &only_pdf).await)
            .get("include")
            .is_none()
    );
    let both = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &both, pdf()).await;
    seed_att(&rig.quota.db, &both, xlsx()).await;
    assert_eq!(
        body_of(&plan(&rig, &both).await)["include"],
        json!([CI_INCLUDE])
    );
}

#[tokio::test]
async fn an_anthropic_chat_gets_no_document_search() {
    // deferred scope: document search for Anthropic chats is not implemented
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_snapshot(multi_catalog())
            .with_providers(multi_providers())
            .with_attachment_tools(settings(KnowledgeSearchConfig::default())),
    )
    .await;
    let chat = rig
        .quota
        .chat_a1(crate::test_support::send::ANTHROPIC_MODEL)
        .await;
    seed_vector_store(&rig.quota.db, &chat, Some(STORE), "azure_openai", at(0)).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    let body = body_of(&plan(&rig, &chat).await);
    assert!(tool_of(&body, "file_search").is_none(), "{body}");
    assert!(
        body.get("tools").is_none(),
        "no document search tool for an Anthropic chat: {body}"
    );
}

// ── knowledge search ─────────────────────────────────────────────────────────

async fn knowledge_rig() -> SendRig {
    send_rig_with(SendRigOptions::new().with_attachment_tools(settings(knowledge_config()))).await
}

#[tokio::test]
async fn knowledge_search_is_off_by_default() {
    // inst-ks-disabled
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert!(kinds(&plan(&rig, &chat).await).is_empty());
}

#[tokio::test]
async fn an_enabled_knowledge_search_adds_the_function_tool_and_its_guard() {
    // inst-ks-definition, inst-ks-guard: `query` required, `top_k` optional
    let rig = knowledge_rig().await;
    let chat = rig.std_chat().await;
    let plan = plan(&rig, &chat).await;
    assert_eq!(kinds(&plan), vec![ToolKind::SearchKnowledge]);
    assert_eq!(plan.tools.guards, vec![knowledge_config().guard]);
    assert_eq!(
        plan.tools.feature, "none",
        "`search_knowledge` is not a built-in tool"
    );
    let body = body_of(&plan);
    let tool = tool_of(&body, "function").expect("a function tool");
    assert_eq!(tool["name"], "search_knowledge");
    assert_eq!(tool["parameters"]["required"], json!(["query"]));
    assert_eq!(tool["parameters"]["properties"]["query"]["type"], "string");
    assert_eq!(tool["parameters"]["properties"]["top_k"]["type"], "integer");
}

#[tokio::test]
async fn knowledge_search_adds_no_reserve_surcharge() {
    // inst-ks-guard: no reserve surcharge
    let off = rig().await;
    let on = knowledge_rig().await;
    let chat_off = off.std_chat().await;
    let chat_on = on.std_chat().await;
    assert_eq!(
        plan(&on, &chat_on).await.decision.reserve_tokens,
        plan(&off, &chat_off).await.decision.reserve_tokens
    );
}

#[tokio::test]
async fn file_search_wins_and_the_two_tools_are_never_in_one_request() {
    // inst-ks-exclusive
    let rig = knowledge_rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, pdf()).await;
    let plan = plan(&rig, &chat).await;
    assert_eq!(kinds(&plan), vec![ToolKind::FileSearch]);
    let body = body_of(&plan);
    assert!(tool_of(&body, "function").is_none(), "{body}");
    assert!(
        !plan
            .tools
            .guards
            .iter()
            .any(|guard| guard == &knowledge_config().guard)
    );
}

#[tokio::test]
async fn an_xlsx_only_chat_with_knowledge_enabled_gets_code_interpreter_and_search_knowledge() {
    let rig = knowledge_rig().await;
    let chat = bound_chat(&rig).await;
    seed_att(&rig.quota.db, &chat, xlsx()).await;
    let plan = plan(&rig, &chat).await;
    assert_eq!(
        kinds(&plan),
        vec![ToolKind::CodeInterpreter, ToolKind::SearchKnowledge]
    );
}

#[tokio::test]
async fn the_provider_registry_of_the_rig_is_unchanged_by_the_knowledge_provider() {
    // the knowledge provider is configured separately from the chat model's provider
    assert!(rig_providers().entry("kb").is_none());
}
