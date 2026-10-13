//! Model-catalog and provider-request fixtures.

use mini_chat_sdk::{ModelApiParams, ModelCatalogEntry};
use serde_json::json;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::infra::llm::{
    ChatTarget, ContentPart, InputItem, ProviderRequest, RequestMetadata, Role,
};

/// A chat target of `kind` at alias `llm.test` serving `/v1/responses`.
pub fn chat_target(kind: ProviderKind) -> ChatTarget {
    ChatTarget {
        provider_id: "openai".to_owned(),
        kind,
        alias: "llm.test".to_owned(),
        api_path_template: "/v1/responses".to_owned(),
    }
}

/// A streaming chat request with one user text message, no tools and default API params.
pub fn provider_request() -> ProviderRequest {
    let tenant_id = Uuid::from_u128(0xA1);
    let user_id = Uuid::from_u128(0xB2);
    ProviderRequest {
        model: "gpt-test".to_owned(),
        instructions: "Be brief.".to_owned(),
        input: vec![InputItem::Message {
            role: Role::User,
            parts: vec![ContentPart::Text("hi".to_owned())],
        }],
        tools: vec![],
        max_output_tokens: 1024,
        max_tool_calls: 2,
        api_params: ModelApiParams {
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: vec![],
            extra_body: None,
            reasoning_effort: None,
        },
        user: format!("{}{}", tenant_id.simple(), user_id.simple()),
        metadata: RequestMetadata {
            tenant_id,
            user_id,
            chat_id: Some(Uuid::from_u128(0xC3)),
            request_type: "chat",
            feature: "none".to_owned(),
        },
        stream: true,
    }
}

/// JSON of a minimal valid catalog entry, as an operator would write it in YAML.
pub fn catalog_entry_json(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {
                "temperature": 0.7,
                "top_p": 1.0,
                "frequency_penalty": 0.0,
                "presence_penalty": 0.0,
                "stop": []
            },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": {
                "web_search": true,
                "file_search": true,
                "image_generation": false,
                "code_interpreter": false,
                "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": true,
                "responses": true,
                "embeddings": false,
                "image_generation": false,
                "audio_speech_generation": false,
                "audio_transcription": false,
                "audio_translation": false
            }
        }
    })
}

/// A minimal valid catalog entry.
pub fn catalog_entry(id: &str) -> ModelCatalogEntry {
    serde_json::from_value(catalog_entry_json(id)).expect("valid fixture")
}

/// A usage event of `tenant` (system task, so it needs no user or turn).
pub fn usage_event(tenant: uuid::Uuid) -> mini_chat_sdk::UsageEvent {
    mini_chat_sdk::UsageEvent {
        tenant_id: tenant,
        user_id: None,
        chat_id: uuid::Uuid::from_u128(2),
        turn_id: None,
        request_id: uuid::Uuid::from_u128(3),
        effective_model: "gpt-4.1-mini".to_owned(),
        selected_model: "gpt-4.1-mini".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "system_task".to_owned(),
        usage: None,
        actual_credits_micro: 0,
        settlement_method: "none".to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        requester_type: "system".to_owned(),
        dedupe_key: "k".to_owned(),
        system_task_type: Some("thread_summary_update".to_owned()),
    }
}

/// A `turn_retry` audit event of `tenant`; `request` identifies it in assertions.
pub fn mutation_audit_event(tenant: uuid::Uuid, request: uuid::Uuid) -> mini_chat_sdk::AuditEvent {
    mini_chat_sdk::AuditEvent::Mutation(mini_chat_sdk::TurnMutationAuditEvent {
        event_type: "turn_retry".to_owned(),
        tenant_id: tenant,
        actor_user_id: uuid::Uuid::from_u128(2),
        chat_id: uuid::Uuid::from_u128(3),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(request),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
    })
}
