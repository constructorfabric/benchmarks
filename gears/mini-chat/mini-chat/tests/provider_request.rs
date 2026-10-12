#![allow(clippy::unwrap_used, clippy::expect_used)]

//! What the gear sends to the OpenAI-compatible provider through the (fake) OAGW: route, body,
//! identity metadata and tools (DESIGN sections 3.2 "`llm_provider`", 3.7 "Provider Request
//! Metadata", 3.4 context assembly and B.1 provider entries). Only fields DESIGN states are asserted.

mod common;

use common::{
    PREMIUM_MODEL, STANDARD_MODEL, SYSTEM_PROMPT, catalog_entry, chat_request_body, complete_turn,
    env_two_models, env_two_models_with_config, env_with_policy, expect_stream, input_position,
    input_text, merge, new_chat, policy, send, send_text, system_prompt_of, terminal,
};
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

const PROVIDER_PREMIUM: &str = "provider-gpt-premium";
const PROVIDER_STANDARD: &str = "provider-gpt-standard";

async fn one_turn_env() -> (TestEnv, TestUser, String) {
    let env = env_two_models().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

fn tools_of(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn tool_type(tool: &Value) -> String {
    tool["type"].as_str().unwrap_or_default().to_owned()
}

// ===========================================================================
// Route
// ===========================================================================

#[tokio::test]
async fn a_turn_issues_exactly_one_chat_request() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(env.provider().chat_requests().len(), 1);
}

#[tokio::test]
async fn the_chat_request_is_a_post_to_the_default_api_path_via_the_provider_alias() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let request = &env.provider().chat_requests()[0];
    assert_eq!(request.method.to_uppercase(), "POST");
    assert!(request.path.ends_with("/v1/responses"), "{}", request.path);
    assert!(
        request.path.contains("llm.test"),
        "the request goes through the provider alias (the host): {}",
        request.path
    );
}

#[tokio::test]
async fn a_configured_api_path_is_used() {
    let env = env_two_models_with_config(json!({
        "providers": { "openai": { "api_path": "/custom/v2/responses" } }
    }))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let request = &env.provider().chat_requests()[0];
    assert!(
        request.path.ends_with("/custom/v2/responses"),
        "{}",
        request.path
    );
}

#[tokio::test]
async fn the_model_placeholder_of_the_api_path_is_replaced_by_the_provider_model_id() {
    let env = env_two_models_with_config(json!({
        "providers": { "openai": { "api_path": "/deployments/{model}/responses" } }
    }))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let request = &env.provider().chat_requests()[0];
    assert!(
        request
            .path
            .contains(&format!("/deployments/{PROVIDER_PREMIUM}/responses")),
        "{}",
        request.path
    );
}

#[tokio::test]
async fn the_request_body_is_json() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let request = &env.provider().chat_requests()[0];
    assert!(
        request.body_json.is_some(),
        "{:?}",
        String::from_utf8_lossy(&request.body_bytes)
    );
}

// ===========================================================================
// Model, streaming, output cap, system prompt
// ===========================================================================

#[tokio::test]
async fn the_body_names_the_provider_model_id_of_the_catalog_entry() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(chat_request_body(&env, 0)["model"], json!(PROVIDER_PREMIUM));
}

#[tokio::test]
async fn the_body_asks_for_a_streamed_response() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(chat_request_body(&env, 0)["stream"], json!(true));
}

#[tokio::test]
async fn max_output_tokens_is_the_catalog_value_when_it_is_below_the_streaming_cap() {
    let env = env_with_policy(policy(vec![
        merge(
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            json!({ "max_output_tokens": 2048 }),
        ),
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(chat_request_body(&env, 0)["max_output_tokens"], json!(2048));
}

#[tokio::test]
async fn max_output_tokens_is_the_streaming_cap_when_it_is_below_the_catalog_value() {
    let env =
        env_two_models_with_config(json!({ "streaming": { "max_output_tokens": 1000 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(chat_request_body(&env, 0)["max_output_tokens"], json!(1000));
}

#[tokio::test]
async fn max_output_tokens_defaults_to_the_catalog_value() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(chat_request_body(&env, 0)["max_output_tokens"], json!(4096));
}

#[tokio::test]
async fn the_catalog_system_prompt_is_sent_as_the_system_instructions() {
    let env = env_with_policy(policy(vec![
        merge(
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            json!({ "system_prompt": "SYSTEM-PROMPT-MARKER-4711" }),
        ),
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let prompt = system_prompt_of(&chat_request_body(&env, 0));
    assert!(prompt.contains("SYSTEM-PROMPT-MARKER-4711"), "{prompt}");
}

#[tokio::test]
async fn the_default_catalog_system_prompt_is_sent_on_every_turn() {
    let (env, user, chat) = one_turn_env().await;
    complete_turn(&env, &user, &chat, "first", "answer").await;
    common::send_ok(&env, &user, &chat, "second").await;

    for index in 0..2 {
        let prompt = system_prompt_of(&chat_request_body(&env, index));
        assert!(prompt.contains(SYSTEM_PROMPT), "request #{index}: {prompt}");
    }
}

// ===========================================================================
// Input: current message last, history in chronological order
// ===========================================================================

#[tokio::test]
async fn the_current_user_message_is_the_last_input_item() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "the current question").await);

    let body = chat_request_body(&env, 0);
    let input = body["input"]
        .as_array()
        .expect("input is an array of messages");
    let last = input.last().unwrap();
    assert_eq!(last["role"], json!("user"), "{last}");
    assert!(last.to_string().contains("the current question"), "{last}");
}

#[tokio::test]
async fn the_first_turn_sends_no_history() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "the current question").await);

    let body = chat_request_body(&env, 0);
    let user_items = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == json!("user"))
        .count();
    assert_eq!(user_items, 1, "{body}");
}

#[tokio::test]
async fn prior_turns_are_sent_in_chronological_order_before_the_current_message() {
    let (env, user, chat) = one_turn_env().await;
    complete_turn(&env, &user, &chat, "Q-ONE", "A-ONE").await;
    complete_turn(&env, &user, &chat, "Q-TWO", "A-TWO").await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let body = chat_request_body(&env, 2);
    let positions: Vec<usize> = ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO", "Q-NOW"]
        .iter()
        .map(|needle| input_position(&body, needle))
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "history is chronological, the current message last: {body}"
    );
    let last = body["input"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    assert!(last.contains("Q-NOW"), "{last}");
}

#[tokio::test]
async fn history_keeps_the_user_and_assistant_roles() {
    let (env, user, chat) = one_turn_env().await;
    complete_turn(&env, &user, &chat, "Q-ONE", "A-ONE").await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let body = chat_request_body(&env, 1);
    let roles: Vec<String> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| matches!(item["role"].as_str(), Some("user" | "assistant")))
        .map(|item| item["role"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{body}");
}

#[tokio::test]
async fn another_chats_messages_are_never_part_of_the_input() {
    let (env, user, chat) = one_turn_env().await;
    let other = new_chat(&env, &user).await;
    complete_turn(
        &env,
        &user,
        &other,
        "OTHER-CHAT-QUESTION",
        "OTHER-CHAT-ANSWER",
    )
    .await;

    complete_turn(&env, &user, &chat, "THIS-CHAT-QUESTION", "answer").await;

    let text = input_text(&chat_request_body(&env, 1));
    assert!(!text.contains("OTHER-CHAT"), "{text}");
}

#[tokio::test]
async fn the_recent_messages_limit_bounds_the_history() {
    let env =
        env_two_models_with_config(json!({ "context": { "recent_messages_limit": 2 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    complete_turn(&env, &user, &chat, "Q-ONE", "A-ONE").await;
    complete_turn(&env, &user, &chat, "Q-TWO", "A-TWO").await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let text = input_text(&chat_request_body(&env, 2));
    assert!(!text.contains("Q-ONE") && !text.contains("A-ONE"), "{text}");
    assert!(text.contains("Q-TWO") && text.contains("A-TWO"), "{text}");
    assert!(text.contains("Q-NOW"), "{text}");
}

#[tokio::test]
async fn a_recent_messages_limit_of_zero_sends_no_history() {
    let env =
        env_two_models_with_config(json!({ "context": { "recent_messages_limit": 0 } })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    complete_turn(&env, &user, &chat, "Q-ONE", "A-ONE").await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let text = input_text(&chat_request_body(&env, 1));
    assert!(!text.contains("Q-ONE") && !text.contains("A-ONE"), "{text}");
    assert!(text.contains("Q-NOW"), "{text}");
}

// ===========================================================================
// Identity: user and metadata
// ===========================================================================

#[tokio::test]
async fn the_user_field_is_the_tenant_and_user_ids_in_simple_form() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let body = chat_request_body(&env, 0);
    let expected = format!("{}{}", user.tenant_id.simple(), user.user_id.simple());
    assert_eq!(body["user"], json!(expected));
    assert_eq!(
        expected.len(),
        64,
        "OpenAI rejects a user longer than 64 characters"
    );
}

#[tokio::test]
async fn the_metadata_object_carries_identity_and_the_chat_request_type() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let metadata = chat_request_body(&env, 0)["metadata"].clone();
    let uuid_of = |key: &str| Uuid::parse_str(metadata[key].as_str().unwrap_or_default()).unwrap();
    assert_eq!(uuid_of("tenant_id"), user.tenant_id, "{metadata}");
    assert_eq!(uuid_of("user_id"), user.user_id, "{metadata}");
    assert_eq!(
        uuid_of("chat_id"),
        Uuid::parse_str(&chat).unwrap(),
        "{metadata}"
    );
    assert_eq!(metadata["request_type"], json!("chat"));
    assert_eq!(metadata["feature"], json!("none"));
}

#[tokio::test]
async fn the_identity_of_the_request_comes_from_the_calling_user() {
    let env = env_two_models().await;
    let alice = TestUser::new();
    let bob = TestUser::in_tenant(alice.tenant_id);
    let alice_chat = new_chat(&env, &alice).await;
    let bob_chat = new_chat(&env, &bob).await;

    expect_stream(&send_text(&env, &alice, &alice_chat, "hello").await);
    expect_stream(&send_text(&env, &bob, &bob_chat, "hello").await);

    let alice_user = chat_request_body(&env, 0)["user"]
        .as_str()
        .unwrap()
        .to_owned();
    let bob_user = chat_request_body(&env, 1)["user"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(alice_user, bob_user);
    assert!(bob_user.ends_with(&bob.user_id.simple().to_string()));
}

// ===========================================================================
// Tools
// ===========================================================================

#[tokio::test]
async fn no_tools_are_sent_without_attachments_and_without_web_search() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(
        tools_of(&chat_request_body(&env, 0)).is_empty(),
        "{}",
        chat_request_body(&env, 0)
    );
}

#[tokio::test]
async fn an_explicitly_disabled_web_search_sends_no_tool() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "web_search": { "enabled": false } }),
        )
        .await,
    );

    assert!(tools_of(&chat_request_body(&env, 0)).is_empty());
}

#[tokio::test]
async fn an_enabled_web_search_sends_the_web_search_tool_with_the_catalog_context_size() {
    let env = env_with_policy(policy(vec![
        merge(
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            json!({ "web_search_context_size": "high" }),
        ),
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done");
    let body = chat_request_body(&env, 0);
    let tools = tools_of(&body);
    let web = tools
        .iter()
        .find(|tool| tool_type(tool).starts_with("web_search"))
        .unwrap_or_else(|| panic!("a web_search tool is expected: {body}"));
    let has_context_size = web
        .as_object()
        .unwrap()
        .iter()
        .any(|(key, value)| key.contains("context_size") && value == &json!("high"));
    assert!(
        has_context_size,
        "the catalog web_search_context_size is sent: {web}"
    );
}

#[tokio::test]
async fn an_enabled_web_search_is_reflected_in_the_request_metadata_feature() {
    let (env, user, chat) = one_turn_env().await;

    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(
        chat_request_body(&env, 0)["metadata"]["feature"],
        json!("web_search")
    );
}

#[tokio::test]
async fn web_search_is_not_sent_to_a_model_without_web_search_support() {
    let mut no_web_search = catalog_entry(PREMIUM_MODEL, "Premium", true, true);
    no_web_search["general_config"]["tool_support"]["web_search"] = json!(false);
    let env = env_with_policy(policy(vec![
        no_web_search,
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(
        terminal(&events).event,
        "done",
        "the request is accepted, the tool is left out"
    );
    assert!(tools_of(&chat_request_body(&env, 0)).is_empty());
}

#[tokio::test]
async fn the_web_search_flag_applies_to_its_own_turn_only() {
    let (env, user, chat) = one_turn_env().await;
    expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    expect_stream(&send_text(&env, &user, &chat, "plain follow-up").await);

    assert!(!tools_of(&chat_request_body(&env, 0)).is_empty());
    assert!(tools_of(&chat_request_body(&env, 1)).is_empty());
}

#[tokio::test]
async fn a_standard_chat_sends_the_standard_provider_model() {
    let env = env_two_models().await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await,
    );

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        chat_request_body(&env, 0)["model"],
        json!(PROVIDER_STANDARD)
    );
}
