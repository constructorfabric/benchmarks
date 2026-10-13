//! Knowledge search (agentic loop) and unexpected function calls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use mini_chat_sdk::UsageTokens;
use uuid::Uuid;

use super::test_helpers::*;
use super::*;
use crate::config::{ProviderKind, StorageKind};
use crate::domain::error::stream_codes;
use crate::domain::service::test_support::{
    Script, TENANT_A, TestEnv, TestOptions, USER_A1, ctx_a1,
};
use crate::infra::llm::{KnowledgeChunk, LlmCompletion, LlmEvent, StorageError, ToolSpec};

const KB_STORE: &str = "vs_knowledge_base";

fn enable_ks(o: &mut TestOptions) {
    let ks = &mut o.cfg.knowledge_search;
    ks.enabled = true;
    ks.vector_store_id = Some(KB_STORE.into());
    ks.provider_id = Some("openai".into());
    let p = o.cfg.providers.get_mut("openai").unwrap();
    p.storage_kind = StorageKind::Azure;
    p.api_version = Some("2025-04-01-preview".into());
}

async fn ks_env(f: impl FnOnce(&mut TestOptions)) -> TestEnv {
    env_with(|o| {
        enable_ks(o);
        f(o);
    })
    .await
}

fn usage(i: i64, o: i64) -> Option<UsageTokens> {
    Some(UsageTokens {
        input_tokens: i,
        output_tokens: o,
        ..UsageTokens::default()
    })
}

fn completion(i: i64, o: i64) -> LlmEvent {
    LlmEvent::Completed(LlmCompletion {
        response_id: Some("resp_kb".into()),
        usage: usage(i, o),
        ..LlmCompletion::default()
    })
}

fn call(id: &str, name: &str, args: &str) -> LlmEvent {
    LlmEvent::FunctionCall {
        call_id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

/// A provider request that ends with function calls.
fn calls(evs: Vec<LlmEvent>) -> Script {
    let mut evs = evs;
    evs.push(completion(10, 5));
    Script::Events {
        events: evs,
        delay: Duration::ZERO,
    }
}

async fn send_collect(env: &TestEnv, chat: Uuid) -> (Vec<crate::api::rest::dto::MiniChatSseEvent>, Uuid) {
    let evs = collect(
        env.services
            .stream
            .send(&ctx_a1(), chat, input("What is our policy?"))
            .await
            .unwrap(),
    )
    .await;
    let rid = started_request_id(&evs);
    (evs, rid)
}

fn has_search_tool(req: &LlmRequest) -> bool {
    req.tools
        .iter()
        .any(|t| matches!(t, ToolSpec::Function { name, .. } if name == SEARCH_KNOWLEDGE))
}

#[test]
fn knowledge_params_require_every_prerequisite() {
    let base = || {
        let mut o = TestOptions::default();
        enable_ks(&mut o);
        o.cfg
    };
    let build = |cfg: &crate::config::MiniChatConfig, retriever: bool| {
        knowledge_params(cfg, &crate::infra::llm::ProviderResolver::new(cfg), retriever, TENANT_A)
    };
    let cfg = base();
    let p = build(&cfg, true).expect("params");
    assert_eq!(p.provider_id, "openai");
    assert_eq!(p.vector_store_id, KB_STORE);
    assert_eq!((p.max_calls_per_message, p.top_k, p.max_chunk_chars), (3, 5, 2000));
    // Retriever not configured.
    assert!(build(&cfg, false).is_none());
    // Disabled.
    let mut c = base();
    c.knowledge_search.enabled = false;
    assert!(build(&c, true).is_none());
    // No api_version.
    let mut c = base();
    c.providers.get_mut("openai").unwrap().api_version = Some(" ".into());
    assert!(build(&c, true).is_none());
    // Wrong kind.
    let mut c = base();
    c.providers.get_mut("openai").unwrap().kind = ProviderKind::VllmResponses;
    assert!(build(&c, true).is_none());
    let mut c = base();
    c.providers.get_mut("openai").unwrap().kind = ProviderKind::AnthropicMessages;
    assert!(build(&c, true).is_some());
    // Unknown provider entry.
    let mut c = base();
    c.knowledge_search.provider_id = Some("missing".into());
    assert!(build(&c, true).is_none());
}

#[test]
fn tool_schema_and_output_format() {
    let ToolSpec::Function {
        name, parameters, ..
    } = search_knowledge_tool()
    else {
        panic!("function tool expected");
    };
    assert_eq!(name, "search_knowledge");
    assert_eq!(parameters["properties"]["query"]["type"], "string");
    assert_eq!(parameters["properties"]["top_k"]["type"], "integer");
    assert_eq!(parameters["required"], serde_json::json!(["query"]));
    let out = format_knowledge_output(
        &[KnowledgeChunk {
            text: "héllo world".into(),
            filename: Some("kb.md".into()),
            score: Some(0.5),
        }],
        5,
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"][0]["text"], "héllo");
    assert_eq!(v["results"][0]["filename"], "kb.md");
    assert_eq!(v["results"][0]["score"], 0.5);
}

#[tokio::test]
async fn agentic_loop_runs_retrieval_and_continues() {
    let env = ks_env(|o| o.cfg.knowledge_search.max_chunk_chars = 4).await;
    *env.knowledge.chunks.lock() = vec![KnowledgeChunk {
        text: "Policy text that is long".into(),
        filename: Some("policy.md".into()),
        score: Some(0.9),
    }];
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(calls(vec![call(
        "call_1",
        "search_knowledge",
        r#"{"query":"policy","top_k":50}"#,
    )]));
    let (evs, rid) = send_collect(&env, chat).await;
    assert_eq!(names(&evs).last(), Some(&"done"), "{:?}", names(&evs));
    // No tool events for the Responses function tool.
    assert!(!names(&evs).contains(&"tool"));
    // Final iteration's usage only.
    assert_eq!(data(&evs, "done")["usage"]["input_tokens"], 100);

    let reqs = env.llm.requests.lock().clone();
    assert_eq!(reqs.len(), 2);
    assert!(has_search_tool(&reqs[0]));
    assert!(reqs[0]
        .instructions
        .ends_with(crate::config::DEFAULT_KNOWLEDGE_SEARCH_GUARD));
    assert!(reqs[0].tool_exchanges.is_empty());
    assert_eq!(reqs[0].metadata["feature"], "search_knowledge");
    // Only the function tool: no max_tool_calls.
    assert_eq!(reqs[0].max_tool_calls, None);
    let x = &reqs[1].tool_exchanges;
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].call_id, "call_1");
    assert_eq!(x[0].name, "search_knowledge");
    let out: serde_json::Value = serde_json::from_str(&x[0].output).unwrap();
    assert_eq!(out["results"][0]["text"], "Poli");
    assert_eq!(reqs[1].input, reqs[0].input);

    // top_k capped at knowledge_search.top_k.
    let kc = env.knowledge.calls.lock().clone();
    assert_eq!(kc, vec![("openai".into(), KB_STORE.into(), "policy".into(), 5)]);

    let t = turn(&env, chat, rid).await;
    assert_eq!(t.state, "completed");
    assert_eq!(t.file_search_completed_count, 1);
    let m = messages(&env, chat).await;
    assert_eq!(m[1].content, "Hello world");
    let u = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(u[0]["file_search_calls"], 1);
    assert_eq!(u[0]["usage"]["input_tokens"], 100);
    env.shutdown().await;
}

#[tokio::test]
async fn search_limit_reached_output_after_max_calls() {
    let env = ks_env(|o| o.cfg.knowledge_search.max_calls_per_message = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(calls(vec![call("c1", "search_knowledge", r#"{"query":"a"}"#)]));
    env.llm.push(calls(vec![call("c2", "search_knowledge", r#"{"query":"b"}"#)]));
    let (evs, rid) = send_collect(&env, chat).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    let reqs = env.llm.requests.lock().clone();
    assert_eq!(reqs.len(), 3);
    let x = &reqs[2].tool_exchanges;
    assert_eq!(x.len(), 2);
    assert_eq!(x[1].output, SEARCH_LIMIT_REACHED);
    assert_eq!(env.knowledge.calls.lock().len(), 1);
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.file_search_completed_count, 1);
    let u = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(u[0]["file_search_calls"], 1);
    env.shutdown().await;
}

#[tokio::test]
async fn iteration_cap_fails_turn() {
    // max_calls = 1 → at most 3 provider requests.
    let env = ks_env(|o| o.cfg.knowledge_search.max_calls_per_message = 1).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    for i in 0..3 {
        env.llm.push(calls(vec![call(&format!("c{i}"), "search_knowledge", r#"{"query":"q"}"#)]));
    }
    let (evs, rid) = send_collect(&env, chat).await;
    assert_eq!(names(&evs).last(), Some(&"error"));
    assert_eq!(data(&evs, "error")["code"], stream_codes::AGENTIC_ITERATIONS_EXCEEDED);
    assert_eq!(env.llm.requests.lock().len(), 3);
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some(stream_codes::AGENTIC_ITERATIONS_EXCEEDED));
    // The last iteration's usage is settled.
    let u = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(u[0]["usage"]["input_tokens"], 10);
    assert_eq!(u[0]["settlement_method"], "actual");
    env.shutdown().await;
}

#[tokio::test]
async fn failed_retrieval_is_counted_as_call_but_not_completed() {
    let env = ks_env(|_| {}).await;
    *env.knowledge.error.lock() = Some(StorageError::Http {
        status: 500,
        message: "down".into(),
    });
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(calls(vec![call("c1", "search_knowledge", r#"{"query":"a"}"#)]));
    let (evs, rid) = send_collect(&env, chat).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    let reqs = env.llm.requests.lock().clone();
    assert!(reqs[1].tool_exchanges[0].output.contains("failed"));
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.file_search_completed_count, 0);
    let u = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(u[0]["file_search_calls"], 1);
    env.shutdown().await;
}

#[tokio::test]
async fn unknown_function_with_knowledge_search_is_unexpected_tool_use() {
    let env = ks_env(|_| {}).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(calls(vec![call("c1", "load_files", "{}")]));
    let (evs, rid) = send_collect(&env, chat).await;
    assert_eq!(data(&evs, "error")["code"], stream_codes::UNEXPECTED_TOOL_USE);
    assert_eq!(env.llm.requests.lock().len(), 1);
    assert!(env.knowledge.calls.lock().is_empty());
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.error_code.as_deref(), Some(stream_codes::UNEXPECTED_TOOL_USE));
    env.shutdown().await;
}

#[tokio::test]
async fn function_call_without_knowledge_search_is_unexpected_tool_use() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(calls(vec![
        LlmEvent::TextDelta("Let me look".into()),
        call("c1", "search_knowledge", r#"{"query":"a"}"#),
    ]));
    let (evs, rid) = send_collect(&env, chat).await;
    assert_eq!(names(&evs).last(), Some(&"error"));
    assert_eq!(data(&evs, "error")["code"], "unexpected_tool_use");
    let reqs = env.llm.requests.lock().clone();
    assert_eq!(reqs.len(), 1);
    assert!(!has_search_tool(&reqs[0]));
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("unexpected_tool_use"));
    let u = env.delivered_to(&env.deps.cfg.outbox.queue_name, 1).await;
    assert_eq!(u[0]["usage"]["input_tokens"], 10);
    env.shutdown().await;
}

#[tokio::test]
async fn file_search_wins_over_knowledge_search() {
    let env = ks_env(|_| {}).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    add_attachment(&env, chat, TENANT_A, Att::default()).await;
    add_vector_store(&env, chat, TENANT_A, "vs_chat0000000000001").await;
    let (evs, _) = send_collect(&env, chat).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    let req = env.llm.requests.lock().last().cloned().unwrap();
    assert!(req.tools.iter().any(|t| matches!(t, ToolSpec::FileSearch { .. })));
    assert!(!has_search_tool(&req));
    assert!(!req
        .instructions
        .contains(crate::config::DEFAULT_KNOWLEDGE_SEARCH_GUARD));
    env.shutdown().await;
}

#[tokio::test]
async fn retrieval_refreshes_turn_progress() {
    let env = ks_env(|_| {}).await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    env.llm.push(calls(vec![call("c1", "search_knowledge", r#"{"query":"a"}"#)]));
    env.llm.push(Script::Hang(vec![]));
    let l = live(
        env.services
            .stream
            .send(&ctx_a1(), chat, input("q"))
            .await
            .unwrap(),
    );
    let rid = turns(&env, chat).await[0].request_id;
    let started = turn(&env, chat, rid).await;
    for _ in 0..100 {
        if env.llm.requests.lock().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(env.llm.requests.lock().len(), 2);
    let t = turn(&env, chat, rid).await;
    assert_eq!(t.state, "running");
    // Successful retrieval is persisted for the orphan watchdog, with updated_at bumped.
    assert_eq!(t.file_search_completed_count, 1);
    assert!(t.updated_at >= started.updated_at);
    assert!(t.last_progress_at >= started.last_progress_at);
    drop(l);
    wait_terminal(&env, chat, rid).await;
    env.shutdown().await;
}
