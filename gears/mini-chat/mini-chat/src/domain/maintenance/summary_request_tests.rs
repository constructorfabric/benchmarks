//! The non-streaming summary request per adapter kind: the exact body fields of
//! `openai_responses` (the Azure variant included), `openai_chat_completions`, `vllm_responses`
//! and `anthropic_messages` (system instructions, input or messages, the output cap field, `store`
//! false on the Responses kinds, `user` or `metadata.user_id`, `metadata` only on
//! `openai_responses`, no tools, no `extra_body`, no sampling parameter of the catalog), the
//! system identity, no credential or provider identifier in the body, and the URI
//! `/{upstream_alias}{api_path}` with only `{model}` replaced, the Azure `api-version` kept and a
//! tenant override or a `host:port` alias honored.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call` (`inst-ad-match`,
//! `inst-ad-openai-responses`, `inst-ad-chat-completions`, `inst-ad-vllm-responses`,
//! `inst-ad-anthropic`, `inst-ad-default`, `inst-ad-no-extras`, `inst-ad-uri`) and
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call` (`inst-call-build`,
//! `inst-call-identity`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-execution` and
//! `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring` (no credential in the gear).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{SummaryRequest, SummaryRequestInput, build_summary_request};
use crate::domain::maintenance::settings::SYSTEM_SUBJECT_ID;
use crate::domain::provider_registry::ProviderRegistry;
use crate::test_support::maintenance::{
    ANTHROPIC_ALIAS, AZURE_ALIAS, AZURE_RESP_ALIAS, CC_ALIAS, SUMMARY_ANTHROPIC, SUMMARY_AZURE,
    SUMMARY_CC, SUMMARY_MODEL, SUMMARY_VLLM, SYSTEM_SUBJECT_HEX, VLLM_ALIAS, catalog, providers,
    providers_json, registry_of, summary_entry,
};
use crate::test_support::{TENANT_A, TENANT_B};

const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0009);
const SYSTEM: &str = "SYSTEM PROMPT TEXT";
const USER: &str = "USER PROMPT TEXT";
const MAX_OUTPUT: u64 = 777;

fn entry(id: &str) -> ModelCatalogEntry {
    let found = catalog()
        .model_catalog
        .into_iter()
        .find(|entry| entry.id == id)
        .expect("catalog entry");
    let mut value = serde_json::to_value(&found).unwrap();
    value["max_output_tokens"] = json!(MAX_OUTPUT);
    serde_json::from_value(value).unwrap()
}

fn request_in(
    registry: &ProviderRegistry,
    model: &ModelCatalogEntry,
    tenant: Uuid,
) -> SummaryRequest {
    let provider = registry.resolve(&model.provider_id, tenant).unwrap();
    build_summary_request(&SummaryRequestInput {
        provider,
        model,
        tenant_id: tenant,
        chat_id: CHAT,
        system_prompt: SYSTEM,
        user_prompt: USER,
    })
}

fn request_for(id: &str) -> SummaryRequest {
    request_in(&providers(), &entry(id), TENANT_A)
}

fn keys(body: &Value) -> BTreeSet<String> {
    body.as_object().unwrap().keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn composite() -> String {
    format!("{}{SYSTEM_SUBJECT_HEX}", TENANT_A.simple())
}

// ── openai_responses ─────────────────────────────────────────────────────────

#[test]
fn the_openai_responses_body_has_exactly_the_specified_fields() {
    // inst-ad-match, inst-ad-openai-responses, inst-call-build
    let body = request_for(SUMMARY_MODEL).body;
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "instructions",
            "input",
            "max_output_tokens",
            "stream",
            "store",
            "user",
            "metadata"
        ]),
        "{body}"
    );
    assert_eq!(body["model"], "provider-summarizer-1");
    assert_eq!(body["instructions"], SYSTEM);
    assert_eq!(body["input"], json!([{ "role": "user", "content": USER }]));
    assert_eq!(body["max_output_tokens"], MAX_OUTPUT);
    assert_eq!(body["stream"], false);
    assert_eq!(body["store"], false);
}

#[test]
fn the_openai_responses_metadata_names_the_summary_request_type() {
    // inst-ad-openai-responses, inst-call-identity
    let body = request_for(SUMMARY_MODEL).body;
    assert_eq!(
        body["metadata"],
        json!({
            "tenant_id": TENANT_A.to_string(),
            "user_id": SYSTEM_SUBJECT_ID.to_string(),
            "chat_id": CHAT.to_string(),
            "request_type": "summary",
            "feature": "none",
        })
    );
}

#[test]
fn the_user_value_is_the_tenant_then_the_system_subject_in_simple_hex() {
    // inst-call-identity: 32 + 32 characters, tenant first
    let body = request_for(SUMMARY_MODEL).body;
    let user = body["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert_eq!(user, composite());
    assert_eq!(&user[..32], TENANT_A.simple().to_string());
    assert_eq!(&user[32..], "111111116a8847689dfc6bcd5187d9ed");
}

#[test]
fn the_responses_body_never_carries_tools_or_provider_state() {
    // inst-ad-openai-responses: no `tools`, no `include`, no `max_tool_calls`, no
    // `previous_response_id`
    for id in [SUMMARY_MODEL, SUMMARY_VLLM, SUMMARY_AZURE] {
        let body = request_for(id).body;
        for forbidden in [
            "tools",
            "tool_choice",
            "include",
            "max_tool_calls",
            "previous_response_id",
        ] {
            assert!(body.get(forbidden).is_none(), "{id}: {forbidden}");
        }
    }
}

#[test]
fn the_azure_variant_differs_only_in_the_uri() {
    // inst-ad-openai-responses: the `api-version` query is carried by `api_path`
    let azure = request_for(SUMMARY_AZURE);
    assert_eq!(
        azure.uri,
        format!(
            "/{AZURE_RESP_ALIAS}/openai/deployments/provider-{SUMMARY_AZURE}/responses?api-version=2025-03-01-preview"
        )
    );
    let mut azure_body = azure.body;
    let plain_body = request_for(SUMMARY_MODEL).body;
    assert_eq!(keys(&azure_body), keys(&plain_body));
    // only the model name differs
    azure_body["model"] = plain_body["model"].clone();
    assert_eq!(azure_body, plain_body);
}

// ── vllm_responses ───────────────────────────────────────────────────────────

#[test]
fn the_vllm_body_is_the_responses_body_without_metadata() {
    // inst-ad-vllm-responses
    let body = request_for(SUMMARY_VLLM).body;
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "instructions",
            "input",
            "max_output_tokens",
            "stream",
            "store",
            "user"
        ]),
        "{body}"
    );
    assert!(body.get("metadata").is_none());
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], false);
    assert_eq!(body["user"], composite());
    assert_eq!(body["input"], json!([{ "role": "user", "content": USER }]));
    assert_eq!(body["max_output_tokens"], MAX_OUTPUT);
}

// ── openai_chat_completions ──────────────────────────────────────────────────

#[test]
fn the_chat_completions_body_has_a_system_and_a_user_message() {
    // inst-ad-chat-completions
    let body = request_for(SUMMARY_CC).body;
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "messages",
            "max_completion_tokens",
            "stream",
            "user"
        ]),
        "{body}"
    );
    assert_eq!(
        body["messages"],
        json!([
            { "role": "system", "content": SYSTEM },
            { "role": "user", "content": USER },
        ])
    );
    assert_eq!(body["max_completion_tokens"], MAX_OUTPUT);
    assert_eq!(body["stream"], false);
    assert_eq!(body["user"], composite());
}

#[test]
fn the_chat_completions_body_has_no_stream_options_metadata_or_tools() {
    // inst-ad-chat-completions
    let body = request_for(SUMMARY_CC).body;
    for forbidden in [
        "stream_options",
        "metadata",
        "tools",
        "tool_choice",
        "max_tokens",
    ] {
        assert!(body.get(forbidden).is_none(), "{forbidden}");
    }
}

// ── anthropic_messages ───────────────────────────────────────────────────────

#[test]
fn the_anthropic_body_has_system_one_user_message_and_the_user_in_metadata() {
    // inst-ad-anthropic
    let body = request_for(SUMMARY_ANTHROPIC).body;
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "system",
            "messages",
            "max_tokens",
            "stream",
            "metadata"
        ]),
        "{body}"
    );
    assert_eq!(body["model"], "provider-summ-ant-1");
    assert_eq!(body["system"], SYSTEM);
    assert_eq!(
        body["messages"],
        json!([{ "role": "user", "content": USER }])
    );
    assert_eq!(body["max_tokens"], MAX_OUTPUT);
    assert_eq!(body["stream"], false);
    assert_eq!(body["metadata"], json!({ "user_id": composite() }));
    assert!(body.get("tools").is_none());
    assert!(body.get("user").is_none());
}

// ── no extras ────────────────────────────────────────────────────────────────

fn with_api_params(id: &str) -> ModelCatalogEntry {
    let mut value = serde_json::to_value(entry(id)).unwrap();
    value["general_config"]["api_params"] = json!({
        "temperature": 0.3, "top_p": 0.9, "frequency_penalty": 0.1, "presence_penalty": 0.2,
        "stop": ["END"], "reasoning_effort": "high",
        "extra_body": { "custom_flag": true, "tools": [{ "type": "x" }] }
    });
    serde_json::from_value(value).unwrap()
}

#[test]
fn no_catalog_api_param_and_no_extra_body_reach_the_summary_request() {
    // inst-ad-no-extras: no sampling parameter, no `reasoning`, no `extra_body` key
    for id in [
        SUMMARY_MODEL,
        SUMMARY_CC,
        SUMMARY_VLLM,
        SUMMARY_ANTHROPIC,
        SUMMARY_AZURE,
    ] {
        let plain = request_in(&providers(), &entry(id), TENANT_A);
        let loaded = request_in(&providers(), &with_api_params(id), TENANT_A);
        assert_eq!(plain, loaded, "{id}: the api params change nothing");
        for forbidden in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "stop",
            "reasoning",
            "reasoning_effort",
            "custom_flag",
            "extra_body",
        ] {
            assert!(loaded.body.get(forbidden).is_none(), "{id}: {forbidden}");
        }
    }
}

#[test]
fn the_output_cap_is_the_catalog_value_not_the_streaming_cap() {
    // inst-call-build: not capped by `streaming.max_output_tokens`
    let mut value = serde_json::to_value(entry(SUMMARY_MODEL)).unwrap();
    value["max_output_tokens"] = json!(123_456);
    let big: ModelCatalogEntry = serde_json::from_value(value).unwrap();
    assert_eq!(
        request_in(&providers(), &big, TENANT_A).body["max_output_tokens"],
        123_456
    );
}

#[test]
fn no_credential_and_no_provider_identifier_is_in_the_body() {
    // inst-ad-no-extras, constraint no-credentials: the body holds the model, the prompts and the
    // identity only; OAGW injects the credentials
    for id in [
        SUMMARY_MODEL,
        SUMMARY_CC,
        SUMMARY_VLLM,
        SUMMARY_ANTHROPIC,
        SUMMARY_AZURE,
    ] {
        let text = request_for(id).body.to_string().to_lowercase();
        for forbidden in [
            "authorization",
            "api_key",
            "api-key",
            "x-api-key",
            "bearer",
            "secret",
            "cred://",
            "azure.example.com",
            "cc.example.com",
            "vllm.example.com",
            "anthropic.example.com",
            "openai.azure.com",
        ] {
            assert!(!text.contains(forbidden), "{id}: {forbidden} in {text}");
        }
    }
}

// ── the URI ──────────────────────────────────────────────────────────────────

#[test]
fn the_uri_is_the_alias_and_the_api_path_of_the_kind() {
    // inst-ad-uri
    assert_eq!(
        request_for(SUMMARY_MODEL).uri,
        format!("/{AZURE_ALIAS}/v1/responses")
    );
    assert_eq!(
        request_for(SUMMARY_CC).uri,
        format!("/{CC_ALIAS}/v1/chat/completions")
    );
    assert_eq!(
        request_for(SUMMARY_VLLM).uri,
        format!("/{VLLM_ALIAS}/v1/responses")
    );
    assert_eq!(
        request_for(SUMMARY_ANTHROPIC).uri,
        format!("/{ANTHROPIC_ALIAS}/v1/messages")
    );
}

#[test]
fn only_the_model_placeholder_is_replaced_and_the_query_is_kept_verbatim() {
    // inst-ad-uri: the Azure `api-version` stays as the `api_path` has it
    let mut config = providers_json();
    config["providers"]["azure_resp"]["api_path"] =
        json!("/openai/deployments/{model}/responses?api-version=2025-03-01-preview&extra={keep}");
    let registry = registry_of(config);
    let uri = request_in(&registry, &entry(SUMMARY_AZURE), TENANT_A).uri;
    assert_eq!(
        uri,
        format!(
            "/{AZURE_RESP_ALIAS}/openai/deployments/provider-{SUMMARY_AZURE}/responses?api-version=2025-03-01-preview&extra={{keep}}"
        )
    );
}

#[test]
fn a_host_port_alias_is_used_as_the_registry_gives_it() {
    // inst-ad-uri: the alias may be `host:port`
    let mut config = providers_json();
    config["providers"]["azure_openai"]["host"] = json!("llm.internal.example");
    config["providers"]["azure_openai"]["port"] = json!(8443);
    let registry = registry_of(config);
    let uri = request_in(&registry, &entry(SUMMARY_MODEL), TENANT_A).uri;
    assert_eq!(uri, "/llm.internal.example:8443/v1/responses");
}

#[test]
fn a_tenant_override_changes_the_alias() {
    // inst-ad-uri: the alias is the one of the provider registry for the chat's tenant
    let mut config = providers_json();
    config["providers"]["azure_openai"]["tenant_overrides"] =
        json!({ TENANT_A.to_string(): { "host": "tenant-a.example.com" } });
    let registry = registry_of(config);
    assert_eq!(
        request_in(&registry, &entry(SUMMARY_MODEL), TENANT_A).uri,
        "/tenant-a.example.com/v1/responses"
    );
    assert_eq!(
        request_in(&registry, &entry(SUMMARY_MODEL), TENANT_B).uri,
        format!("/{AZURE_ALIAS}/v1/responses"),
        "another tenant keeps the entry alias"
    );
}

#[test]
fn the_tenant_and_chat_of_the_request_are_those_of_the_work_item() {
    // inst-call-identity: `metadata.tenant_id` and `metadata.chat_id` follow the inputs
    let other_chat = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0042);
    let registry = providers();
    let model = entry(SUMMARY_MODEL);
    let provider = registry.resolve(&model.provider_id, TENANT_B).unwrap();
    let request = build_summary_request(&SummaryRequestInput {
        provider,
        model: &model,
        tenant_id: TENANT_B,
        chat_id: other_chat,
        system_prompt: SYSTEM,
        user_prompt: USER,
    });
    assert_eq!(request.body["metadata"]["tenant_id"], TENANT_B.to_string());
    assert_eq!(request.body["metadata"]["chat_id"], other_chat.to_string());
    assert_eq!(
        request.body["user"],
        format!("{}{SYSTEM_SUBJECT_HEX}", TENANT_B.simple())
    );
}

#[test]
fn a_catalog_entry_built_by_the_helper_serves_the_tests() {
    // fixture check: the catalog entries of the summary models carry the expected provider ids
    let found = summary_entry("x", "cc", &json!({}));
    assert_eq!(found.provider_id, "cc");
    assert_eq!(found.provider_model_id, "provider-x");
}
