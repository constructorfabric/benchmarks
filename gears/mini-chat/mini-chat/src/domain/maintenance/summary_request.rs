//! The non-streaming summary request per adapter kind
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`, steps `inst-ad-match`,
//! `inst-ad-no-extras` and `inst-ad-uri`): the JSON body of `openai_responses` (Azure included),
//! `openai_chat_completions`, `vllm_responses` and `anthropic_messages`, and the URI
//! `/{upstream_alias}{api_path}` with only `{model}` replaced.

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::{Value, json};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::maintenance::settings::SYSTEM_SUBJECT_ID;
use crate::domain::provider_registry::EffectiveProvider;
use crate::domain::streaming::provider_request::{composite_user, provider_uri};

/// The request the summary call sends: `POST` of `body` to `uri` through OAGW.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryRequest {
    pub uri: String,
    pub body: Value,
}

/// What a summary request is built from.
#[domain_model]
#[derive(Debug, Clone)]
pub struct SummaryRequestInput<'a> {
    /// The provider of the summary model resolved for the chat's tenant.
    pub provider: &'a EffectiveProvider,
    pub model: &'a ModelCatalogEntry,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_prompt: &'a str,
    pub user_prompt: &'a str,
}

/// Build the request of the adapter of `input.provider.kind`.
#[must_use]
pub fn build_summary_request(input: &SummaryRequestInput<'_>) -> SummaryRequest {
    let model = input.model;
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-identity
    // the composite `user` value holds the chat's tenant id and the platform default subject id,
    // tenant first; the metadata of the OpenAI Responses adapter names the tenant, that subject,
    // the chat, `request_type` `summary` and `feature` `none`
    let user = composite_user(input.tenant_id, SYSTEM_SUBJECT_ID);
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-provider-call:p1:inst-call-identity
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-no-extras
    // none of the catalog `general_config.api_params` (no sampling parameter, no `reasoning`
    // effort) and no `extra_body` key is added, and neither a credential nor a provider
    // identifier is placed in the body
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-no-extras
    let responses = |with_metadata: bool| {
        let mut body = json!({
            "model": model.provider_model_id,
            "instructions": input.system_prompt,
            "input": [{ "role": "user", "content": input.user_prompt }],
            "max_output_tokens": model.max_output_tokens,
            "stream": false,
            "store": false,
            "user": user,
        });
        if with_metadata {
            body["metadata"] = json!({
                "tenant_id": input.tenant_id.to_string(),
                "user_id": SYSTEM_SUBJECT_ID.to_string(),
                "chat_id": input.chat_id.to_string(),
                "request_type": "summary",
                "feature": "none",
            });
        }
        body
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-match
    // the configuration validation rejects any other kind at startup, so the match has no
    // default arm
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-default
    let body = match input.provider.kind {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-openai-responses
        ProviderKind::OpenaiResponses => responses(true),
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-openai-responses
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-vllm-responses
        ProviderKind::VllmResponses => responses(false),
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-vllm-responses
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-chat-completions
        ProviderKind::OpenaiChatCompletions => json!({
            "model": model.provider_model_id,
            "messages": [
                { "role": "system", "content": input.system_prompt },
                { "role": "user", "content": input.user_prompt },
            ],
            "max_completion_tokens": model.max_output_tokens,
            "stream": false,
            "user": user,
        }),
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-chat-completions
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-anthropic
        ProviderKind::AnthropicMessages => json!({
            "model": model.provider_model_id,
            "system": input.system_prompt,
            "messages": [{ "role": "user", "content": input.user_prompt }],
            "max_tokens": model.max_output_tokens,
            "stream": false,
            "metadata": { "user_id": user },
        }),
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-anthropic
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-default
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-match
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-uri
    // `/{upstream_alias}{api_path}` with only `{model}` replaced; the query of `api_path` (the
    // Azure `api-version`) is kept verbatim
    SummaryRequest {
        uri: provider_uri(input.provider, &model.provider_model_id),
        body,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-uri
}

#[cfg(test)]
#[path = "summary_request_tests.rs"]
mod summary_request_tests;
