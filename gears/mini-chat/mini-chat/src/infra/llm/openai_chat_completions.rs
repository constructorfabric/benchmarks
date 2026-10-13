//! `openai_chat_completions` adapter: Chat Completions API request body and
//! stream translation (spec §11.5; DESIGN §3.2 `llm_provider`, §3.3 "event:
//! tool", §4 "Provider Request Metadata").
//!
//! Wire format: `POST {api_path}` with `{model, messages, stream,
//! stream_options.include_usage, max_completion_tokens, user, tools}`; the
//! instructions are the first (`system`) message. Built-in tools are dropped,
//! function tools (`search_knowledge`) kept; `metadata` and `max_tool_calls`
//! are not sent. The API has no file inputs, so image parts are dropped.
//! Stream chunks carry no event name; `data: [DONE]` ends the stream.

use serde_json::{Map, Value, json};
use tracing::warn;

use super::openai_responses::error_message;
use super::{
    CompletionResult, FunctionCall, LlmError, LlmEvent, LlmRequest, LlmTool, LlmUsage, ParseState,
    ProviderAdapter, SEARCH_KNOWLEDGE_DESCRIPTION, merge_extra_body, search_knowledge_parameters,
};

/// Client tool event name of a function call (DESIGN §3.3 "event: tool").
const FUNCTION_CALL: &str = "function_call";

/// Chat Completions API adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiChatCompletionsAdapter;

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

fn messages(req: &LlmRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.input.len() + 1);
    if !req.instructions.is_empty() {
        out.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.input {
        if !m.image_file_ids.is_empty() {
            warn!(
                images = m.image_file_ids.len(),
                "Chat Completions has no file inputs; image parts are dropped"
            );
        }
        out.push(json!({"role": m.role.as_str(), "content": m.text}));
    }
    for round in &req.tool_rounds {
        let calls: Vec<Value> = round
            .results
            .iter()
            .map(|r| {
                json!({
                    "id": r.call.call_id,
                    "type": "function",
                    "function": {"name": r.call.name, "arguments": r.call.arguments},
                })
            })
            .collect();
        out.push(json!({"role": "assistant", "content": null, "tool_calls": calls}));
        for r in &round.results {
            out.push(json!({"role": "tool", "tool_call_id": r.call.call_id, "content": r.output}));
        }
    }
    out
}

/// Function tools only; the built-in tools have no Chat Completions form.
fn tools(req: &LlmRequest) -> Vec<Value> {
    req.tools
        .iter()
        .filter_map(|t| match t {
            LlmTool::SearchKnowledge => Some(json!({
                "type": "function",
                "function": {
                    "name": "search_knowledge",
                    "description": SEARCH_KNOWLEDGE_DESCRIPTION,
                    "parameters": search_knowledge_parameters(),
                },
            })),
            LlmTool::FileSearch { .. }
            | LlmTool::WebSearch { .. }
            | LlmTool::CodeInterpreter { .. } => None,
        })
        .collect()
}

fn build(req: &LlmRequest) -> Value {
    let mut body = Map::new();
    merge_extra_body(&mut body, req.api_params.extra_body.as_ref());
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages(req)));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
    body.insert("user".into(), json!(req.user));
    let tools = tools(req);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    let p = &req.api_params;
    for (key, value) in [
        ("temperature", p.temperature),
        ("top_p", p.top_p),
        ("frequency_penalty", p.frequency_penalty),
        ("presence_penalty", p.presence_penalty),
    ] {
        if let Some(v) = value {
            body.insert(key.into(), json!(v));
        }
    }
    if !p.stop.is_empty() {
        body.insert("stop".into(), json!(p.stop));
    }
    if let Some(effort) = p.reasoning_effort.as_deref().filter(|e| !e.is_empty()) {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// response parsing
// ---------------------------------------------------------------------------

fn int_at(v: &Value, path: &[&str]) -> i64 {
    path.iter()
        .try_fold(v, |acc, k| acc.get(k))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// `usage` of a chunk or response (`prompt_tokens` -> input, `completion_tokens` -> output).
fn parse_usage(v: Option<&Value>) -> Option<LlmUsage> {
    let u = v.filter(|u| u.is_object())?;
    Some(LlmUsage {
        input_tokens: int_at(u, &["prompt_tokens"]),
        output_tokens: int_at(u, &["completion_tokens"]),
        cache_read_input_tokens: int_at(u, &["prompt_tokens_details", "cached_tokens"]),
        cache_write_input_tokens: 0,
        reasoning_tokens: int_at(u, &["completion_tokens_details", "reasoning_tokens"]),
    })
}

/// Incomplete reason of a finish reason (`None` for a normal end).
fn incomplete_reason(finish: Option<&str>) -> Option<String> {
    match finish? {
        "stop" | "tool_calls" | "function_call" => None,
        "length" => Some("max_tokens".to_owned()),
        "content_filter" => Some("content_filter".to_owned()),
        _ => Some("other".to_owned()),
    }
}

fn completed(st: &mut ParseState) -> LlmEvent {
    st.terminated = true;
    LlmEvent::Completed {
        usage: st.usage,
        response_id: st.response_id.clone(),
        incomplete_reason: incomplete_reason(st.finish_reason.as_deref()),
    }
}

/// One `tool_calls[]` delta: a new index starts a call (tool `start` event).
fn tool_call_delta(st: &mut ParseState, tc: &Value, out: &mut Vec<LlmEvent>) {
    let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
    let func = tc.get("function");
    let name = func
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = func
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(call) = st.calls.get_mut(&index) {
        call.arguments.push_str(args);
        if call.name.is_empty() {
            name.clone_into(&mut call.name);
        }
        return;
    }
    let call_id = tc
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    out.push(LlmEvent::ToolStart {
        name: FUNCTION_CALL.to_owned(),
        details: json!({"index": index, "call_id": call_id, "name": name}),
    });
    st.calls.insert(
        index,
        FunctionCall {
            call_id,
            name: name.to_owned(),
            arguments: args.to_owned(),
        },
    );
}

/// The finished calls: a `done` tool event and the call itself each.
fn finish_calls(st: &mut ParseState, out: &mut Vec<LlmEvent>) {
    for call in std::mem::take(&mut st.calls).into_values() {
        out.push(LlmEvent::ToolDone {
            name: FUNCTION_CALL.to_owned(),
            details: json!({"call_id": call.call_id, "name": call.name, "arguments": call.arguments}),
        });
        out.push(LlmEvent::FunctionCall(call));
    }
}

fn translate(st: &mut ParseState, data: &Value) -> Vec<LlmEvent> {
    if let Some(err) = data.get("error").filter(|e| !e.is_null()) {
        st.terminated = true;
        return vec![LlmEvent::Failed {
            error: LlmError::Provider {
                message: error_message(err).unwrap_or_else(|| err.to_string()),
            },
            usage: st.usage,
        }];
    }
    if let Some(id) = data.get("id").and_then(Value::as_str) {
        st.response_id = Some(id.to_owned());
    }
    if let Some(u) = parse_usage(data.get("usage")) {
        st.usage = Some(u);
    }
    let mut out = Vec::new();
    for choice in data
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            out.push(LlmEvent::TextDelta(text.to_owned()));
        }
        for tc in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            tool_call_delta(st, tc, &mut out);
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            st.finish_reason = Some(finish.to_owned());
            finish_calls(st, &mut out);
        }
    }
    // The usage chunk follows the finish chunk (`stream_options.include_usage`).
    if st.finish_reason.is_some() && st.usage.is_some() {
        out.push(completed(st));
    }
    out
}

impl ProviderAdapter for OpenAiChatCompletionsAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        build(req)
    }

    fn parse_event(&self, st: &mut ParseState, _event: &str, data: &str) -> Vec<LlmEvent> {
        if st.terminated {
            return Vec::new();
        }
        if data.trim() == "[DONE]" {
            let mut out = Vec::new();
            finish_calls(st, &mut out);
            out.push(completed(st));
            return out;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(v) if v.is_object() => translate(st, &v),
            _ => Vec::new(),
        }
    }

    fn parse_completion(&self, body: &[u8]) -> Result<CompletionResult, LlmError> {
        let v: Value = serde_json::from_slice(body).map_err(|_| LlmError::Provider {
            message: "invalid provider response".to_owned(),
        })?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(LlmError::Provider {
                message: error_message(err)
                    .unwrap_or_else(|| "provider response failed".to_owned()),
            });
        }
        let text = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(CompletionResult {
            text,
            usage: parse_usage(v.get("usage")),
        })
    }
}

#[cfg(test)]
#[path = "openai_chat_completions_tests.rs"]
mod openai_chat_completions_tests;
