//! The provider request of the four adapter kinds (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the URI through OAGW and the JSON body of the
//! `OpenAI` Responses, `OpenAI` Chat Completions, `vLLM` Responses and Anthropic Messages adapters.
//!
//! Algorithms `cpt-cf-mini-chat-algo-message-streaming-provider-call` (URI),
//! `cpt-cf-mini-chat-algo-message-streaming-responses-request` and
//! `cpt-cf-mini-chat-algo-message-streaming-adapter-kinds`.

use serde_json::{Map, Value, json};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::provider_registry::EffectiveProvider;
use crate::domain::streaming::context::{ContextItem, ContextRole, UserItem};
use crate::domain::streaming::plan::ProviderCallPlan;
use crate::domain::streaming::tools::{ToolDefinition, ToolKind};

/// The request the provider task sends through `proxy_request`: `POST` of `body` to `uri`.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRequest {
    /// `/{upstream_alias}{api_path}` with `{model}` replaced by the `provider_model_id` and the
    /// `api_path` query (for example `?api-version=...`) kept verbatim.
    pub uri: String,
    pub body: serde_json::Value,
}

/// The `include` entry of the code interpreter outputs.
const CODE_INTERPRETER_INCLUDE: &str = "code_interpreter_call.outputs";

/// The keys of the request that only the pipeline controls: a catalog `extra_body` never
/// replaces them.
const CONTROLLED_KEYS: [&str; 18] = [
    "model",
    "input",
    "messages",
    "instructions",
    "system",
    "stream",
    "stream_options",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "max_tool_calls",
    "tools",
    "tool_choice",
    "include",
    "store",
    "previous_response_id",
    "user",
    "metadata",
];

/// The request URI: `/{upstream_alias}{api_path}`, only `{model}` substituted.
#[must_use]
pub fn provider_uri(provider: &EffectiveProvider, provider_model_id: &str) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-uri
    // the alias is the one of the provider registry (the tenant override's alias when the
    // tenant has one) and may be `host:port`; the `api_path` query is kept verbatim
    format!(
        "/{}{}",
        provider.upstream_alias,
        provider.api_path.replace("{model}", provider_model_id)
    )
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-uri
}

/// The composite `user` value: the tenant id and the user id in 32-character lowercase
/// hexadecimal form without hyphens, tenant first (exactly 64 characters).
#[must_use]
pub fn composite_user(tenant_id: Uuid, user_id: Uuid) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-user
    format!("{}{}", tenant_id.simple(), user_id.simple())
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-user
}

/// Build the request of the adapter of `plan.provider.kind` from the context plan, the tool set,
/// the decision and the effective model. Never places a credential in the body.
///
/// # Errors
///
/// `Internal` when the request cannot be built.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the request builders of every adapter share one fallible contract"
)]
pub fn build_provider_request(plan: &ProviderCallPlan) -> Result<ProviderRequest, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-match
    let body = match plan.provider.kind {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-openai-responses
        ProviderKind::OpenaiResponses => responses_body(plan, true),
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-openai-responses
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-chat-completions
        ProviderKind::OpenaiChatCompletions => chat_completions_body(plan),
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-chat-completions
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-vllm-responses
        ProviderKind::VllmResponses => responses_body(plan, false),
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-vllm-responses
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-anthropic
        ProviderKind::AnthropicMessages => anthropic_body(plan),
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-anthropic
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-match
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-return
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-return
    Ok(ProviderRequest {
        uri: provider_uri(&plan.provider, &plan.model.provider_model_id),
        body,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-return
}

fn role_name(role: ContextRole) -> &'static str {
    match role {
        ContextRole::User => "user",
        ContextRole::Assistant => "assistant",
    }
}

/// The text items of the plan before the current message: the summary item then the kept
/// history, as `(role, text)` pairs in the order of the plan.
fn history_items(plan: &ProviderCallPlan) -> impl Iterator<Item = &ContextItem> {
    plan.context
        .plan
        .thread_summary
        .iter()
        .chain(plan.context.plan.recent.iter())
}

/// Copy the catalog `extra_body` into `body` without the request-controlled keys.
fn merge_extra_body(plan: &ProviderCallPlan, body: &mut Map<String, Value>) {
    let Some(Value::Object(extra)) = &plan.model.general_config.api_params.extra_body else {
        return;
    };
    for (key, value) in extra {
        if CONTROLLED_KEYS.contains(&key.as_str()) {
            // the key is never logged with its value: only the name
            tracing::warn!(key = %key, "an `extra_body` key controlled by the request is ignored");
        } else {
            body.insert(key.clone(), value.clone());
        }
    }
}

/// The sampling parameters of the catalog that are set, in the field names shared by the
/// `OpenAI` adapters.
fn sampling_params(plan: &ProviderCallPlan, body: &mut Map<String, Value>) {
    let params = &plan.model.general_config.api_params;
    if let Some(temperature) = params.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = params.top_p {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(penalty) = params.frequency_penalty {
        body.insert("frequency_penalty".to_owned(), json!(penalty));
    }
    if let Some(penalty) = params.presence_penalty {
        body.insert("presence_penalty".to_owned(), json!(penalty));
    }
    if !params.stop.is_empty() {
        body.insert("stop".to_owned(), json!(params.stop));
    }
}

// ── OpenAI and vLLM Responses ────────────────────────────────────────────────

/// The current user message of a Responses request: a plain string without an image, else an
/// array with the text part and one `input_image` per image in `attachment_ids` order.
fn responses_current(user: &UserItem) -> Value {
    let content = if user.images.is_empty() {
        json!(user.text)
    } else {
        let mut parts = vec![json!({ "type": "input_text", "text": user.text })];
        parts.extend(
            user.images
                .iter()
                .map(|image| json!({ "type": "input_image", "file_id": image.provider_file_id })),
        );
        Value::Array(parts)
    };
    json!({ "role": "user", "content": content })
}

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-definition
/// The parameters of the `search_knowledge` function tool: `query` (required) and `top_k`.
fn knowledge_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" },
            "top_k": { "type": "integer" },
        },
        "required": ["query"],
    })
}
// @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-contribution:p1:inst-ks-definition

// @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-request
/// One tool of the Responses format.
fn responses_tool(plan: &ProviderCallPlan, tool: &ToolDefinition) -> Value {
    match tool.kind {
        ToolKind::FileSearch => {
            let (ids, max) = plan.tools.file_search.as_ref().map_or_else(
                || (Vec::new(), plan.model.max_num_results),
                |spec| (spec.vector_store_ids.clone(), spec.max_num_results),
            );
            json!({
                "type": "file_search",
                "vector_store_ids": ids,
                "max_num_results": max,
            })
        }
        ToolKind::WebSearch => json!({
            "type": "web_search",
            "search_context_size": tool.search_context_size.unwrap_or(plan.model.web_search_context_size),
        }),
        ToolKind::CodeInterpreter => json!({
            "type": "code_interpreter",
            "container": { "type": "auto", "file_ids": plan.tools.code_interpreter_files },
        }),
        ToolKind::SearchKnowledge => json!({
            "type": "function",
            "name": ToolKind::SearchKnowledge.as_str(),
            "description": "Search the organization knowledge base.",
            "parameters": knowledge_parameters(),
        }),
    }
}
// @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-request

/// The Responses body; `full` selects the `OpenAI` adapter (tools, `metadata`, `max_tool_calls`
/// and `include`) over the `vLLM` one (none of them).
fn responses_body(plan: &ProviderCallPlan, full: bool) -> Value {
    let mut body = Map::new();
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-model
    body.insert("model".to_owned(), json!(plan.model.provider_model_id));
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-model
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-instructions
    // the system prompt followed by the tool guards; never an `input` item
    body.insert(
        "instructions".to_owned(),
        json!(plan.context.plan.system_instructions),
    );
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-instructions
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-input
    let mut input: Vec<Value> = history_items(plan)
        .map(|item| json!({ "role": role_name(item.role), "content": item.text }))
        .collect();
    input.push(responses_current(&plan.context.plan.user));
    body.insert("input".to_owned(), Value::Array(input));
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-input
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-stream-limits
    body.insert("stream".to_owned(), json!(true));
    body.insert(
        "max_output_tokens".to_owned(),
        json!(plan.decision.max_output_tokens_applied),
    );
    if full {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-max-calls
        // the only per-request bound of the built-in tool calls: the catalog `max_tool_calls`
        body.insert(
            "max_tool_calls".to_owned(),
            json!(plan.model.max_tool_calls),
        );
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-assembly:p1:inst-tools-max-calls
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-stream-limits
    if full {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-tools
        // the tool set in its order; omitted when empty
        if !plan.tools.tools.is_empty() {
            let tools: Vec<Value> = plan
                .tools
                .tools
                .iter()
                .map(|tool| responses_tool(plan, tool))
                .collect();
            body.insert("tools".to_owned(), Value::Array(tools));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-tools
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-include
        let mut include = plan.tools.include.clone();
        let has_code_interpreter = plan
            .tools
            .tools
            .iter()
            .any(|tool| tool.kind == ToolKind::CodeInterpreter);
        if has_code_interpreter && !include.iter().any(|e| e == CODE_INTERPRETER_INCLUDE) {
            include.push(CODE_INTERPRETER_INCLUDE.to_owned());
        }
        if !include.is_empty() {
            body.insert("include".to_owned(), json!(include));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-include
    }
    body.insert(
        "user".to_owned(),
        json!(composite_user(plan.tenant_id, plan.user_id)),
    );
    if full {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-metadata
        // observability only
        body.insert(
            "metadata".to_owned(),
            json!({
                "tenant_id": plan.tenant_id.to_string(),
                "user_id": plan.user_id.to_string(),
                "chat_id": plan.chat_id.to_string(),
                "request_type": "chat",
                "feature": plan.tools.feature,
            }),
        );
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-metadata
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-stateless
    // every request carries the full assembled context: no provider-side conversation state
    body.insert("store".to_owned(), json!(false));
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-stateless
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-api-params
    sampling_params(plan, &mut body);
    if let Some(effort) = &plan.model.general_config.api_params.reasoning_effort {
        body.insert("reasoning".to_owned(), json!({ "effort": effort }));
    }
    merge_extra_body(plan, &mut body);
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-api-params
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-no-secrets
    // the body holds the identity and the context only: OAGW injects the credentials
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-responses-request:p1:inst-req-no-secrets
    Value::Object(body)
}

// ── OpenAI Chat Completions ──────────────────────────────────────────────────

fn chat_completions_body(plan: &ProviderCallPlan) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let instructions = &plan.context.plan.system_instructions;
    if !instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": instructions }));
    }
    messages.extend(
        history_items(plan)
            .map(|item| json!({ "role": role_name(item.role), "content": item.text })),
    );
    let user = &plan.context.plan.user;
    let content = if user.images.is_empty() {
        json!(user.text)
    } else {
        let mut parts = vec![json!({ "type": "text", "text": user.text })];
        parts.extend(user.images.iter().map(
            |image| json!({ "type": "image_url", "image_url": { "url": image.provider_file_id } }),
        ));
        Value::Array(parts)
    };
    messages.push(json!({ "role": "user", "content": content }));

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(plan.model.provider_model_id));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), json!(true));
    // the usage chunk of the stream
    body.insert(
        "stream_options".to_owned(),
        json!({ "include_usage": true }),
    );
    body.insert(
        "max_completion_tokens".to_owned(),
        json!(plan.decision.max_output_tokens_applied),
    );
    body.insert(
        "user".to_owned(),
        json!(composite_user(plan.tenant_id, plan.user_id)),
    );
    // function tools only: `file_search`, `web_search` and `code_interpreter` are dropped
    let functions: Vec<Value> = plan
        .tools
        .tools
        .iter()
        .filter(|tool| tool.kind == ToolKind::SearchKnowledge)
        .map(|_| {
            json!({
                "type": "function",
                "function": {
                    "name": ToolKind::SearchKnowledge.as_str(),
                    "description": "Search the organization knowledge base.",
                    "parameters": knowledge_parameters(),
                },
            })
        })
        .collect();
    if !functions.is_empty() {
        body.insert("tools".to_owned(), Value::Array(functions));
    }
    sampling_params(plan, &mut body);
    if let Some(effort) = &plan.model.general_config.api_params.reasoning_effort {
        body.insert("reasoning_effort".to_owned(), json!(effort));
    }
    merge_extra_body(plan, &mut body);
    Value::Object(body)
}

// ── Anthropic Messages ───────────────────────────────────────────────────────

fn anthropic_body(plan: &ProviderCallPlan) -> Value {
    let mut messages: Vec<Value> = history_items(plan)
        .map(|item| json!({ "role": role_name(item.role), "content": item.text }))
        .collect();
    let user = &plan.context.plan.user;
    let content = if user.images.is_empty() {
        json!(user.text)
    } else {
        let mut parts = vec![json!({ "type": "text", "text": user.text })];
        // images are referenced by the secondary Anthropic file id of the attachment; the send
        // preflight rejects an image without one before any provider call
        parts.extend(user.images.iter().filter_map(|image| {
            image.secondary_file_id.as_ref().map(|file_id| {
                json!({ "type": "image", "source": { "type": "file", "file_id": file_id } })
            })
        }));
        Value::Array(parts)
    };
    messages.push(json!({ "role": "user", "content": content }));

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(plan.model.provider_model_id));
    body.insert(
        "system".to_owned(),
        json!(plan.context.plan.system_instructions),
    );
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert(
        "max_tokens".to_owned(),
        json!(plan.decision.max_output_tokens_applied),
    );
    body.insert("stream".to_owned(), json!(true));
    body.insert(
        "metadata".to_owned(),
        json!({ "user_id": composite_user(plan.tenant_id, plan.user_id) }),
    );
    // `web_search` and `code_interpreter` map to the Anthropic server tools, function tools are
    // kept and `file_search` is dropped
    let tools: Vec<Value> = plan
        .tools
        .tools
        .iter()
        .filter_map(|tool| match tool.kind {
            ToolKind::FileSearch => None,
            ToolKind::WebSearch => {
                Some(json!({ "type": "web_search_20250305", "name": "web_search" }))
            }
            ToolKind::CodeInterpreter => {
                Some(json!({ "type": "code_execution_20250522", "name": "code_execution" }))
            }
            ToolKind::SearchKnowledge => Some(json!({
                "name": ToolKind::SearchKnowledge.as_str(),
                "description": "Search the organization knowledge base.",
                "input_schema": knowledge_parameters(),
            })),
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    // no `extra_body` on this adapter
    Value::Object(body)
}
