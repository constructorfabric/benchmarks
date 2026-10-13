//! What the gear sends to the provider through OAGW for a chat turn: the
//! `openai_responses` request contract.
//!
//! Source of truth: DESIGN 3.2 `llm_provider` / 3.5 (requests go through
//! `ServiceGatewayClientV1` to `{alias}{api_path}`; credentials are injected
//! by OAGW), 4 "Provider Request Metadata" (`user`, `metadata`), 4 "Context Plan
//! Assembly" (system prompt, tool guards, recent messages, current message),
//! 4 "Web Search Configuration" / "File Search Tool Availability", the model
//! catalog field table (`provider_model_id`, `max_output_tokens`,
//! `max_tool_calls`, `api_params`, `web_search_context_size`,
//! `max_num_results`) and B.4 / B.5.1 (`streaming.max_output_tokens`).
//!
//! The system prompt is accepted either as the `instructions` field or as a
//! leading `system` / `developer` item of `input`
//! (`RecordedProviderRequest::system_text`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod stream_support;

use serde_json::{Value, json};
use stream_support::{
    API_PATH, FILE_GUARD, PROVIDER_ALIAS, SYSTEM_PROMPT, StreamHarness, StreamOptions, TENANT_A,
    USER_A, WEB_GUARD, hex, norm_id, premium_with, stream_model_json,
};

async fn one_turn(h: &StreamHarness, body: Value) -> stream_support::RecordedProviderRequest {
    let chat = h.seed_chat().await;
    let sent = h.send(chat, &body).await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.events);
    h.oagw.last_request()
}

// ── transport ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_request_is_a_json_post_to_the_alias_and_api_path_without_credentials() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.method, http::Method::POST);
    assert_eq!(req.path(), format!("/{PROVIDER_ALIAS}{API_PATH}"));
    assert!(
        req.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json")),
        "headers: {:?}",
        req.headers
    );
    for credential_header in ["authorization", "api-key", "x-api-key"] {
        assert!(
            req.headers.get(credential_header).is_none(),
            "mini-chat never handles provider credentials, OAGW injects `{credential_header}`"
        );
    }
    assert_eq!(h.oagw.request_count(), 1, "one provider call per turn");
}

#[tokio::test]
async fn the_api_path_of_the_provider_entry_is_honoured() {
    let h = StreamHarness::with(StreamOptions {
        config: json!({"providers": {"openai": {"api_path": "/openai/v1/responses?api-version=preview"}}}),
        ..Default::default()
    })
    .await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.path(), format!("/{PROVIDER_ALIAS}/openai/v1/responses"));
    assert_eq!(req.uri.query(), Some("api-version=preview"));
}

// ── identity, model, limits ──────────────────────────────────────────────────

#[tokio::test]
async fn the_body_names_the_provider_model_and_asks_for_a_stream() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["model"], json!("provider-model-gpt-premium"));
    assert_ne!(
        req.body["model"],
        json!("gpt-premium"),
        "not the catalog id"
    );
    assert_eq!(req.body["stream"], json!(true));
    assert!(
        req.body["input"].is_array(),
        "input is an item array: {}",
        req.body
    );
}

#[tokio::test]
async fn user_is_the_tenant_and_user_ids_in_simple_form_64_characters() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    let user = req.body["user"].as_str().expect("`user` is sent");
    assert_eq!(user.len(), 64);
    assert_eq!(user, format!("{}{}", hex(TENANT_A), hex(USER_A)));
}

#[tokio::test]
async fn metadata_names_tenant_user_chat_request_type_and_feature() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, "hello").await;
    assert_eq!(sent.terminal().event, "done");
    let meta = &h.oagw.last_request().body["metadata"];
    let norm = |key: &str| {
        norm_id(
            meta[key]
                .as_str()
                .unwrap_or_else(|| panic!("metadata.{key}: {meta}")),
        )
    };
    assert_eq!(norm("tenant_id"), hex(TENANT_A));
    assert_eq!(norm("user_id"), hex(USER_A));
    assert_eq!(norm("chat_id"), hex(chat));
    assert_eq!(meta["request_type"], json!("chat"));
    assert_eq!(meta["feature"], json!("none"));
    assert!(
        meta.as_object().unwrap().values().all(Value::is_string),
        "provider metadata values are strings: {meta}"
    );
}

#[tokio::test]
async fn max_output_tokens_is_the_smaller_of_the_catalog_and_the_streaming_cap() {
    // catalog 4096 < cap 32768
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["max_output_tokens"], json!(4096));

    // cap 1000 < catalog 4096
    let h = StreamHarness::with(StreamOptions {
        config: json!({"streaming": {"max_output_tokens": 1000}}),
        ..Default::default()
    })
    .await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["max_output_tokens"], json!(1000));

    // catalog 256 < cap
    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"max_output_tokens": 256})),
        ..Default::default()
    })
    .await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["max_output_tokens"], json!(256));
}

#[tokio::test]
async fn max_tool_calls_comes_from_the_catalog() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["max_tool_calls"], json!(3));
}

#[tokio::test]
async fn sampling_parameters_are_sent_only_when_the_catalog_sets_them() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["temperature"], json!(0.7));
    assert_eq!(req.body["top_p"], json!(0.9));
    assert_eq!(req.body["frequency_penalty"], json!(0.1));
    assert_eq!(req.body["presence_penalty"], json!(0.2));

    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"general_config": {"api_params": {
            "temperature": null, "top_p": null, "frequency_penalty": null, "presence_penalty": null
        }}})),
        ..Default::default()
    })
    .await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
    ] {
        assert!(
            req.body.get(key).is_none(),
            "`{key}` must be absent for reasoning models that reject it: {}",
            req.body
        );
    }

    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"general_config": {"api_params": {
            "temperature": 0.2, "top_p": null, "frequency_penalty": null, "presence_penalty": null
        }}})),
        ..Default::default()
    })
    .await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(req.body["temperature"], json!(0.2));
    assert!(req.body.get("top_p").is_none());
}

#[tokio::test]
async fn extra_body_is_merged_at_the_top_level_except_the_keys_the_gear_controls() {
    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"general_config": {"api_params": {"extra_body": {
            "service_tier": "flex",
            "model": "evil-model",
            "stream": false,
            "tools": [{"type": "evil"}],
            "user": "evil-user",
            "metadata": {"evil": "1"},
            "max_output_tokens": 1,
            "store": true
        }}}})),
        ..Default::default()
    })
    .await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert_eq!(
        req.body["service_tier"],
        json!("flex"),
        "free keys are merged"
    );
    assert_eq!(req.body["model"], json!("provider-model-gpt-premium"));
    assert_eq!(req.body["stream"], json!(true));
    assert_eq!(req.body["max_output_tokens"], json!(4096));
    assert_ne!(req.body["user"], json!("evil-user"));
    assert_ne!(req.body["metadata"], json!({"evil": "1"}));
    assert!(
        req.tool("evil").is_none(),
        "controlled keys of extra_body are ignored"
    );
    assert_ne!(req.body.get("store"), Some(&json!(true)));
}

// ── input: system prompt, history, current message ───────────────────────────

#[tokio::test]
async fn the_system_prompt_of_the_catalog_is_sent_and_no_guards_without_tools() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    let system = req.system_text();
    assert!(system.contains(SYSTEM_PROMPT), "system text: {system}");
    assert!(!system.contains(WEB_GUARD));
    assert!(!system.contains(FILE_GUARD));
    assert!(
        req.conversation().iter().all(|(role, _)| role != "system"),
        "the system prompt is not part of the conversation items"
    );
}

#[tokio::test]
async fn a_first_turn_sends_only_the_current_user_message() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "Current question?"})).await;
    assert_eq!(
        req.conversation(),
        vec![("user".to_owned(), "Current question?".to_owned())]
    );
}

#[tokio::test]
async fn recent_history_is_sent_in_chronological_order_with_the_current_message_last() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_history(chat, 2).await;
    h.say(chat, "Current question?").await;
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![
            "user:question 1",
            "assistant:answer 1",
            "user:question 2",
            "assistant:answer 2",
            "user:Current question?"
        ]
    );
}

#[tokio::test]
async fn the_second_turn_of_a_conversation_carries_the_first_one() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(stream_support::ProviderReply::text(&["First", " answer"]));
    h.say(chat, "First question").await;
    h.say(chat, "Second question").await;
    let second = h.oagw.last_request();
    assert_eq!(
        second.conversation_texts(),
        vec![
            "user:First question",
            "assistant:First answer",
            "user:Second question"
        ]
    );
    assert_eq!(h.oagw.request_count(), 2);
}

// ── tools ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn no_tools_are_sent_by_default() {
    let h = StreamHarness::new().await;
    let req = one_turn(&h, json!({"content": "hello"})).await;
    assert!(req.tools().is_empty(), "tools: {:?}", req.tools());
}

#[tokio::test]
async fn web_search_is_sent_with_the_catalog_context_size_and_its_guard() {
    for size in ["low", "medium", "high"] {
        let h = StreamHarness::with(StreamOptions {
            models: premium_with(json!({"web_search_context_size": size})),
            ..Default::default()
        })
        .await;
        let chat = h.seed_chat().await;
        let sent = h
            .send(
                chat,
                &json!({"content": "news?", "web_search": {"enabled": true}}),
            )
            .await;
        assert_eq!(sent.terminal().event, "done");
        let req = h.oagw.last_request();
        let tool = req
            .tool("web_search")
            .unwrap_or_else(|| panic!("tools: {:?}", req.tools()));
        assert_eq!(tool["search_context_size"], json!(size));
        assert!(
            req.system_text().contains(WEB_GUARD),
            "the guard accompanies the tool"
        );
        assert!(req.system_text().contains(SYSTEM_PROMPT));
        assert_eq!(req.body["metadata"]["feature"], json!("web_search"));
        let turn = h.turn_of(sent.request_id()).await.unwrap();
        assert!(
            turn.web_search_enabled,
            "the request flag is stored on the turn"
        );
    }
}

#[tokio::test]
async fn web_search_is_not_sent_unless_the_request_enables_it() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.send(
        chat,
        &json!({"content": "a", "web_search": {"enabled": false}}),
    )
    .await;
    h.say(chat, "b").await;
    for req in h.oagw.requests() {
        assert!(req.tool("web_search").is_none(), "{:?}", req.tools());
        assert!(!req.system_text().contains(WEB_GUARD));
        assert_eq!(req.body["metadata"]["feature"], json!("none"));
    }
}

#[tokio::test]
async fn web_search_is_skipped_for_a_model_without_web_search_support() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat_on("gpt-standard").await; // tool_support.web_search = false
    let sent = h
        .send(
            chat,
            &json!({"content": "news?", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(
        sent.terminal().event,
        "done",
        "the turn proceeds without the tool"
    );
    let req = h.oagw.last_request();
    assert!(req.tool("web_search").is_none());
    assert!(
        !req.system_text().contains(WEB_GUARD),
        "no guard without the tool"
    );
    assert_eq!(req.body["metadata"]["feature"], json!("none"));
    assert!(
        h.turn_of(sent.request_id())
            .await
            .unwrap()
            .web_search_enabled,
        "the requested flag is still stored"
    );
}

#[tokio::test]
async fn file_search_needs_a_ready_document_and_the_chat_vector_store() {
    let h = StreamHarness::new().await;

    // no attachments at all
    let chat = h.seed_chat().await;
    h.say(chat, "a").await;
    assert!(h.oagw.last_request().tool("file_search").is_none());

    // a ready document but no vector store row yet
    let chat = h.seed_chat().await;
    h.seed_document(chat, "a.pdf").await;
    h.say(chat, "b").await;
    assert!(h.oagw.last_request().tool("file_search").is_none());

    // a vector store but no ready document
    let chat = h.seed_chat().await;
    h.chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "p.pdf").status("pending"))
        .await;
    h.seed_vector_store(chat, "vs_seededstore0001").await;
    h.say(chat, "c").await;
    assert!(h.oagw.last_request().tool("file_search").is_none());

    // a deleted document does not count
    let chat = h.seed_chat().await;
    h.chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "d.pdf").deleted())
        .await;
    h.seed_vector_store(chat, "vs_seededstore0002").await;
    h.say(chat, "d").await;
    assert!(h.oagw.last_request().tool("file_search").is_none());
}

#[tokio::test]
async fn file_search_references_the_chat_vector_store_without_filters() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_document(chat, "contract.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0003").await;
    let sent = h.say(chat, "what does the contract say?").await;
    assert_eq!(sent.terminal().event, "done");

    let req = h.oagw.last_request();
    let tool = req
        .tool("file_search")
        .unwrap_or_else(|| panic!("tools: {:?}", req.tools()));
    assert_eq!(tool["vector_store_ids"], json!(["vs_seededstore0003"]));
    assert_eq!(tool["max_num_results"], json!(5), "catalog max_num_results");
    assert!(
        tool.get("filters").is_none(),
        "no metadata filtering in P1: {tool}"
    );
    assert!(req.system_text().contains(FILE_GUARD));
    assert_eq!(req.body["metadata"]["feature"], json!("file_search"));
    assert!(
        !sent.blob().contains("vs_seededstore0003"),
        "the vector store id never reaches the client"
    );
}

#[tokio::test]
async fn file_search_is_also_sent_without_attachment_ids_on_the_message() {
    // retrieval always covers the whole chat (DESIGN 3.3 Retrieval Model)
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_document(chat, "contract.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0004").await;
    h.send(
        chat,
        &json!({"content": "no attachment ids", "attachment_ids": []}),
    )
    .await;
    assert!(h.oagw.last_request().tool("file_search").is_some());
}

#[tokio::test]
async fn file_and_web_search_are_reported_together_in_the_metadata_feature() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_document(chat, "contract.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0005").await;
    h.send(
        chat,
        &json!({"content": "both", "web_search": {"enabled": true}}),
    )
    .await;
    let req = h.oagw.last_request();
    assert_eq!(
        req.body["metadata"]["feature"],
        json!("file_search+web_search")
    );
    assert!(req.tool("file_search").is_some() && req.tool("web_search").is_some());
    assert!(req.system_text().contains(FILE_GUARD) && req.system_text().contains(WEB_GUARD));
}

#[tokio::test]
async fn file_search_is_omitted_when_the_model_or_the_kill_switch_forbids_it() {
    // the model's tool_support.file_search is false
    let mut no_file_search = stream_model_json("gpt-premium", "Premium");
    no_file_search["general_config"]["tool_support"]["file_search"] = json!(false);
    let h = StreamHarness::with(StreamOptions {
        models: vec![no_file_search],
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.seed_document(chat, "a.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0006").await;
    h.say(chat, "x").await;
    assert!(h.oagw.last_request().tool("file_search").is_none());

    // the kill switch disable_file_search
    let h = StreamHarness::new().await;
    h.policy
        .set_kill_switches(json!({"disable_file_search": true}));
    let chat = h.seed_chat().await;
    h.seed_document(chat, "a.pdf").await;
    h.seed_vector_store(chat, "vs_seededstore0007").await;
    let sent = h.say(chat, "x").await;
    assert_eq!(
        sent.terminal().event,
        "done",
        "the turn proceeds without retrieval"
    );
    let req = h.oagw.last_request();
    assert!(req.tool("file_search").is_none());
    assert!(!req.system_text().contains(FILE_GUARD));
}

#[tokio::test]
async fn the_provider_request_of_each_chat_stays_in_its_own_chat() {
    // history and vector store of another chat never leak into the request
    let h = StreamHarness::new().await;
    let busy = h.seed_chat().await;
    h.seed_history(busy, 3).await;
    h.seed_document(busy, "secret.pdf").await;
    h.seed_vector_store(busy, "vs_seededstore0008").await;
    let quiet = h.seed_chat().await;
    h.say(quiet, "just me").await;
    let req = h.oagw.last_request();
    assert_eq!(req.conversation_texts(), vec!["user:just me"]);
    assert!(req.tool("file_search").is_none());
    assert_eq!(
        norm_id(req.body["metadata"]["chat_id"].as_str().unwrap()),
        hex(quiet)
    );
}
