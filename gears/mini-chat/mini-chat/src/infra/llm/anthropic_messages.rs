//! `anthropic_messages` adapter: Anthropic Messages API request body and
//! stream translation (spec §11.5; DESIGN §3.2 `llm_provider`, §3.3 "Provider
//! Event Translation" and "event: tool", §4 "Provider Request Metadata",
//! "Model Catalog Configuration").
//!
//! Wire format: `{model, system, messages, max_tokens, stream, metadata.user_id}`
//! with the `anthropic-version: 2023-06-01` header. `file_search` is dropped,
//! `web_search` / `code_interpreter` become the `web_search_20250305` /
//! `code_execution_20250522` server tools, `search_knowledge` a client tool.
//! `extra_body`, `max_tool_calls`, the penalties and `reasoning_effort` are not
//! sent. Images are `file` sources with the Anthropic (secondary) file ids the
//! stream service put on the message (images without one are dropped there).

use serde_json::{Map, Value, json};

use super::openai_responses::error_message;
use super::{
    CompletionResult, FunctionCall, LlmError, LlmEvent, LlmMessage, LlmRequest, LlmTool, LlmUsage,
    ParseState, ProviderAdapter, RawCitation, SEARCH_KNOWLEDGE_DESCRIPTION,
    search_knowledge_parameters,
};

/// `anthropic-version` header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta enabling `file` image sources and the Files API.
pub const FILES_API_BETA: &str = "files-api-2025-04-14";
/// Beta enabling the code execution server tool.
pub const CODE_EXECUTION_BETA: &str = "code-execution-2025-05-22";

const WEB_SEARCH_TOOL: &str = "web_search_20250305";
const CODE_EXECUTION_TOOL: &str = "code_execution_20250522";

/// Anthropic Messages API adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessagesAdapter;

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

fn message_json(m: &LlmMessage) -> Value {
    let content = if m.image_file_ids.is_empty() {
        Value::String(m.text.clone())
    } else {
        let mut blocks = vec![json!({"type": "text", "text": m.text})];
        blocks.extend(
            m.image_file_ids
                .iter()
                .map(|id| json!({"type": "image", "source": {"type": "file", "file_id": id}})),
        );
        Value::Array(blocks)
    };
    json!({"role": m.role.as_str(), "content": content})
}

fn tool_json(t: &LlmTool) -> Option<Value> {
    match t {
        LlmTool::FileSearch { .. } => None,
        LlmTool::WebSearch { .. } => Some(json!({"type": WEB_SEARCH_TOOL, "name": "web_search"})),
        LlmTool::CodeInterpreter { .. } => {
            Some(json!({"type": CODE_EXECUTION_TOOL, "name": "code_execution"}))
        }
        LlmTool::SearchKnowledge => Some(json!({
            "name": "search_knowledge",
            "description": SEARCH_KNOWLEDGE_DESCRIPTION,
            "input_schema": search_knowledge_parameters(),
        })),
    }
}

fn build(req: &LlmRequest) -> Value {
    let mut messages: Vec<Value> = req.input.iter().map(message_json).collect();
    for round in &req.tool_rounds {
        let uses: Vec<Value> = round
            .results
            .iter()
            .map(|r| {
                let input = serde_json::from_str::<Value>(&r.call.arguments)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                json!({"type": "tool_use", "id": r.call.call_id, "name": r.call.name, "input": input})
            })
            .collect();
        let results: Vec<Value> = round
            .results
            .iter()
            .map(|r| json!({"type": "tool_result", "tool_use_id": r.call.call_id, "content": r.output}))
            .collect();
        messages.push(json!({"role": "assistant", "content": uses}));
        messages.push(json!({"role": "user", "content": results}));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    body.insert("metadata".into(), json!({"user_id": req.user}));
    let tools: Vec<Value> = req.tools.iter().filter_map(tool_json).collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    let p = &req.api_params;
    for (key, value) in [("temperature", p.temperature), ("top_p", p.top_p)] {
        if let Some(v) = value {
            body.insert(key.into(), json!(v));
        }
    }
    if !p.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(p.stop));
    }
    Value::Object(body)
}

fn headers(req: &LlmRequest) -> Vec<(&'static str, String)> {
    let mut betas = Vec::new();
    if req.input.iter().any(|m| !m.image_file_ids.is_empty()) {
        betas.push(FILES_API_BETA);
    }
    if req
        .tools
        .iter()
        .any(|t| matches!(t, LlmTool::CodeInterpreter { .. }))
    {
        betas.push(CODE_EXECUTION_BETA);
    }
    let mut out = vec![("anthropic-version", ANTHROPIC_VERSION.to_owned())];
    if !betas.is_empty() {
        out.push(("anthropic-beta", betas.join(",")));
    }
    out
}

// ---------------------------------------------------------------------------
// response parsing
// ---------------------------------------------------------------------------

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Normalized usage: Anthropic reports cache reads and writes beside
/// `input_tokens`; here they are a subset of it (DESIGN §5.5.9).
fn parse_usage(v: Option<&Value>) -> Option<LlmUsage> {
    let u = v.filter(|u| u.is_object())?;
    let cache_read = int(u, "cache_read_input_tokens");
    let cache_write = int(u, "cache_creation_input_tokens");
    Some(LlmUsage {
        input_tokens: int(u, "input_tokens") + cache_read + cache_write,
        output_tokens: int(u, "output_tokens"),
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: 0,
    })
}

/// `message_delta.usage`: output tokens are cumulative; input fields, when
/// present, replace the `message_start` values.
fn merge_delta_usage(st: &mut ParseState, u: &Value) {
    let mut usage = st.usage.unwrap_or_default();
    if u.get("input_tokens").is_some()
        && let Some(parsed) = parse_usage(Some(u))
    {
        usage.input_tokens = parsed.input_tokens;
        usage.cache_read_input_tokens = parsed.cache_read_input_tokens;
        usage.cache_write_input_tokens = parsed.cache_write_input_tokens;
    }
    if let Some(out) = u.get("output_tokens").and_then(Value::as_i64) {
        usage.output_tokens = out;
    }
    st.usage = Some(usage);
}

/// Client tool event name of a function tool (DESIGN §3.3 "event: tool").
fn function_tool_event(name: &str) -> &'static str {
    match name {
        "search_knowledge" => "search_knowledge",
        "load_files" => "load_files",
        _ => "unknown_tool",
    }
}

/// Shared tool name of a server tool.
fn server_tool(name: &str) -> Option<&'static str> {
    match name {
        "web_search" => Some("web_search"),
        "code_execution" => Some("code_interpreter"),
        _ => None,
    }
}

/// Incomplete reason of a stop reason (`None` for a normal end).
fn incomplete_reason(stop: Option<&str>) -> Option<String> {
    match stop? {
        "max_tokens" => Some("max_tokens".to_owned()),
        "refusal" => Some("content_filter".to_owned()),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn block_start(st: &mut ParseState, index: u64, block: &Value) -> Vec<LlmEvent> {
    let name = str_field(block, "name").unwrap_or_default();
    match str_field(block, "type") {
        Some("tool_use") => {
            st.calls.insert(
                index,
                FunctionCall {
                    call_id: str_field(block, "id").unwrap_or_default().to_owned(),
                    name: name.to_owned(),
                    arguments: String::new(),
                },
            );
            vec![LlmEvent::ToolStart {
                name: function_tool_event(name).to_owned(),
                details: json!({}),
            }]
        }
        Some("server_tool_use") => match server_tool(name) {
            Some(tool) => {
                st.blocks.insert(index, tool.to_owned());
                vec![LlmEvent::ToolStart {
                    name: tool.to_owned(),
                    details: json!({}),
                }]
            }
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn block_delta(st: &mut ParseState, index: u64, delta: &Value) -> Vec<LlmEvent> {
    match str_field(delta, "type") {
        Some("text_delta") => str_field(delta, "text")
            .filter(|t| !t.is_empty())
            .map(|t| LlmEvent::TextDelta(t.to_owned()))
            .into_iter()
            .collect(),
        Some("input_json_delta") => {
            if let (Some(call), Some(part)) =
                (st.calls.get_mut(&index), str_field(delta, "partial_json"))
            {
                call.arguments.push_str(part);
            }
            Vec::new()
        }
        Some("citations_delta") => delta
            .get("citation")
            .and_then(|c| web_citation(st, c))
            .into_iter()
            .collect(),
        // Thinking is not forwarded: only the vLLM adapter emits reasoning.
        _ => Vec::new(),
    }
}

/// A web search result citation (deduplicated).
fn web_citation(st: &mut ParseState, c: &Value) -> Option<LlmEvent> {
    if str_field(c, "type") != Some("web_search_result_location") {
        return None;
    }
    let url = str_field(c, "url")?.to_owned();
    let snippet = str_field(c, "cited_text").unwrap_or_default().to_owned();
    st.citations_seen
        .insert(format!("web|{url}|{snippet}"))
        .then(|| {
            LlmEvent::Citation(RawCitation::Web {
                title: str_field(c, "title").unwrap_or_default().to_owned(),
                url,
                snippet,
                span: None,
            })
        })
}

fn block_stop(st: &mut ParseState, index: u64) -> Vec<LlmEvent> {
    if let Some(mut call) = st.calls.remove(&index) {
        if call.arguments.trim().is_empty() {
            "{}".clone_into(&mut call.arguments);
        }
        return vec![LlmEvent::FunctionCall(call)];
    }
    st.blocks
        .remove(&index)
        .map(|name| LlmEvent::ToolDone {
            name,
            details: json!({}),
        })
        .into_iter()
        .collect()
}

fn failed(st: &mut ParseState, err: &Value) -> Vec<LlmEvent> {
    st.terminated = true;
    let message = error_message(err).unwrap_or_else(|| err.to_string());
    let error = if str_field(err, "type") == Some("rate_limit_error") {
        LlmError::RateLimited {
            retry_after_secs: None,
            message,
        }
    } else {
        LlmError::Provider { message }
    };
    vec![LlmEvent::Failed {
        error,
        usage: st.usage,
    }]
}

fn translate(st: &mut ParseState, name: &str, data: &Value) -> Vec<LlmEvent> {
    let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
    match name {
        "message_start" => {
            let msg = data.get("message").unwrap_or(&Value::Null);
            if let Some(id) = str_field(msg, "id") {
                st.response_id = Some(id.to_owned());
            }
            if let Some(u) = parse_usage(msg.get("usage")) {
                st.usage = Some(u);
            }
            Vec::new()
        }
        "content_block_start" => data
            .get("content_block")
            .map(|b| block_start(st, index, b))
            .unwrap_or_default(),
        "content_block_delta" => data
            .get("delta")
            .map(|d| block_delta(st, index, d))
            .unwrap_or_default(),
        "content_block_stop" => block_stop(st, index),
        "message_delta" => {
            if let Some(stop) = data.get("delta").and_then(|d| str_field(d, "stop_reason")) {
                st.finish_reason = Some(stop.to_owned());
            }
            if let Some(u) = data.get("usage").filter(|u| u.is_object()) {
                merge_delta_usage(st, u);
            }
            Vec::new()
        }
        "message_stop" => {
            st.terminated = true;
            vec![LlmEvent::Completed {
                usage: st.usage,
                response_id: st.response_id.clone(),
                incomplete_reason: incomplete_reason(st.finish_reason.as_deref()),
            }]
        }
        "error" => failed(st, data.get("error").unwrap_or(data)),
        _ => Vec::new(),
    }
}

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        build(req)
    }

    fn headers(&self, req: &LlmRequest) -> Vec<(&'static str, String)> {
        headers(req)
    }

    fn parse_event(&self, st: &mut ParseState, event: &str, data: &str) -> Vec<LlmEvent> {
        if st.terminated {
            return Vec::new();
        }
        let parsed = serde_json::from_str::<Value>(data).ok();
        let name = if event.is_empty() || event == "message" {
            parsed.as_ref().and_then(|v| str_field(v, "type"))
        } else {
            Some(event)
        };
        match (name, parsed.as_ref()) {
            (Some(name), Some(v)) if v.is_object() => translate(st, name, v),
            (Some("error"), _) => failed(st, &Value::String(data.to_owned())),
            _ => Vec::new(),
        }
    }

    fn parse_completion(&self, body: &[u8]) -> Result<CompletionResult, LlmError> {
        let v: Value = serde_json::from_slice(body).map_err(|_| LlmError::Provider {
            message: "invalid provider response".to_owned(),
        })?;
        if str_field(&v, "type") == Some("error") {
            let err = v.get("error").unwrap_or(&v);
            return Err(LlmError::Provider {
                message: error_message(err)
                    .unwrap_or_else(|| "provider response failed".to_owned()),
            });
        }
        let text = v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|b| str_field(b, "type") == Some("text"))
            .filter_map(|b| str_field(b, "text"))
            .collect();
        Ok(CompletionResult {
            text,
            usage: parse_usage(v.get("usage")),
        })
    }
}

#[cfg(test)]
#[path = "anthropic_messages_tests.rs"]
mod anthropic_messages_tests;
