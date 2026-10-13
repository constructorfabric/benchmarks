//! `openai_responses` adapter: `OpenAI` / Azure `OpenAI` Responses API request body
//! (spec §11.2) and event translation (DESIGN §3.3 Provider Event Translation).

use super::{
    CompletionResult, FunctionCall, LlmError, LlmEvent, LlmMessage, LlmRequest, LlmTool, LlmUsage,
    ParseState, ProviderAdapter, RawCitation, SEARCH_KNOWLEDGE_DESCRIPTION, merge_extra_body,
    search_knowledge_parameters,
};
use crate::domain::model::MessageRole;
use serde_json::{Map, Value, json};

/// Request keys the adapter controls; `extra_body` cannot override them.
pub const CONTROLLED_KEYS: &[&str] = &[
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

/// Cap of the `code_interpreter` tool `output` (characters).
const CODE_OUTPUT_MAX_CHARS: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// `OpenAI` Responses API adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiResponsesAdapter;

// ---------------------------------------------------------------------------
// request
// ---------------------------------------------------------------------------

fn message_json(m: &LlmMessage, content_array: bool) -> Value {
    let content = if content_array || !m.image_file_ids.is_empty() {
        let mut items = vec![json!({"type": "input_text", "text": m.text})];
        items.extend(
            m.image_file_ids
                .iter()
                .map(|id| json!({"type": "input_image", "file_id": id})),
        );
        Value::Array(items)
    } else {
        Value::String(m.text.clone())
    };
    json!({"role": m.role.as_str(), "content": content})
}

fn tool_json(t: &LlmTool) -> Value {
    match t {
        LlmTool::FileSearch {
            vector_store_id,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": [vector_store_id],
            "max_num_results": max_num_results,
        }),
        LlmTool::WebSearch { context_size } => json!({
            "type": "web_search",
            "search_context_size": context_size,
        }),
        LlmTool::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": file_ids},
        }),
        LlmTool::SearchKnowledge => json!({
            "type": "function",
            "name": "search_knowledge",
            "description": SEARCH_KNOWLEDGE_DESCRIPTION,
            "parameters": search_knowledge_parameters(),
        }),
    }
}

/// Request body; also the base of the vLLM Responses body.
pub(super) fn build(req: &LlmRequest) -> Value {
    let mut body = Map::new();
    merge_extra_body(&mut body, req.api_params.extra_body.as_ref());

    let last = req.input.len().saturating_sub(1);
    let mut input: Vec<Value> = req
        .input
        .iter()
        .enumerate()
        .map(|(i, m)| message_json(m, i == last && m.role == MessageRole::User))
        .collect();
    // Finished knowledge-search rounds: each call, then its output.
    for round in &req.tool_rounds {
        for r in &round.results {
            input.push(json!({
                "type": "function_call",
                "call_id": r.call.call_id,
                "name": r.call.name,
                "arguments": r.call.arguments,
            }));
        }
        for r in &round.results {
            input.push(json!({
                "type": "function_call_output",
                "call_id": r.call.call_id,
                "output": r.output,
            }));
        }
    }

    body.insert("model".into(), json!(req.model));
    body.insert("instructions".into(), json!(req.instructions));
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), json!(req.stream));
    body.insert("store".into(), json!(false));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(tool_json).collect()),
        );
    }
    if req.tools.iter().any(LlmTool::is_builtin)
        && let Some(n) = req.max_tool_calls
    {
        body.insert("max_tool_calls".into(), json!(n));
    }
    if req
        .tools
        .iter()
        .any(|t| matches!(t, LlmTool::CodeInterpreter { .. }))
    {
        body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
    }
    body.insert("user".into(), json!(req.user));
    let md = &req.metadata;
    body.insert(
        "metadata".into(),
        json!({
            "tenant_id": md.tenant_id.to_string(),
            "user_id": md.user_id.to_string(),
            "chat_id": md.chat_id.to_string(),
            "request_type": md.request_type.as_str(),
            "feature": md.feature,
        }),
    );

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
        body.insert("reasoning".into(), json!({"effort": effort}));
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// response parsing
// ---------------------------------------------------------------------------

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `usage` object of a response (`None` when absent or not an object).
fn parse_usage(v: Option<&Value>) -> Option<LlmUsage> {
    let u = v.filter(|u| u.is_object())?;
    let details = |key: &str, field: &str| {
        u.get(key)
            .and_then(|d| d.get(field))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    Some(LlmUsage {
        input_tokens: int(u, "input_tokens"),
        output_tokens: int(u, "output_tokens"),
        cache_read_input_tokens: details("input_tokens_details", "cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: details("output_tokens_details", "reasoning_tokens"),
    })
}

/// Message of an error value: `message`, else `code`, else a string error.
pub(super) fn error_message(err: &Value) -> Option<String> {
    match err {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| o.get("code").and_then(Value::as_str))
            .map(str::to_owned),
        _ => None,
    }
}

/// Error of a `response.failed` payload: `response.error`, else top-level `error`.
fn failed_message(data: &Value) -> Option<String> {
    data.get("response")
        .and_then(|r| r.get("error"))
        .and_then(error_message)
        .or_else(|| data.get("error").and_then(error_message))
}

fn provider_failed(message: String, usage: Option<LlmUsage>) -> LlmEvent {
    LlmEvent::Failed {
        error: LlmError::Provider { message },
        usage,
    }
}

/// Key of an output text part.
fn part_key(data: &Value) -> String {
    let item = data
        .get("item_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            data.get("output_index")
                .and_then(Value::as_u64)
                .map(|i| i.to_string())
        })
        .unwrap_or_default();
    let content = data
        .get("content_index")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    format!("{item}:{content}")
}

fn tool_start(tool: &str) -> Vec<LlmEvent> {
    vec![LlmEvent::ToolStart {
        name: tool.to_owned(),
        details: json!({}),
    }]
}

fn span_of(a: &Value) -> Option<(u32, u32)> {
    let idx = |k: &str| {
        a.get(k)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    Some((idx("start_index")?, idx("end_index")?))
}

/// Characters `[start, end)` of `text`; empty when the range is outside it.
fn char_range(text: &str, (start, end): (u32, u32)) -> String {
    let (start, end) = (start as usize, end as usize);
    if start > end || end > text.chars().count() {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

/// Map one annotation (deduplicated) to a citation event.
fn citation(st: &mut ParseState, ann: &Value, part_text: &str) -> Option<LlmEvent> {
    let span = span_of(ann);
    let (key, raw) = match ann.get("type").and_then(Value::as_str)? {
        "url_citation" => {
            let url = ann.get("url").and_then(Value::as_str)?.to_owned();
            let snippet = ann.get("text").and_then(Value::as_str).map_or_else(
                || span.map(|s| char_range(part_text, s)).unwrap_or_default(),
                str::to_owned,
            );
            (
                format!("web|{url}|{span:?}"),
                RawCitation::Web {
                    title: ann
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    url,
                    snippet,
                    span,
                },
            )
        }
        "file_citation" => {
            let file_id = ann.get("file_id").and_then(Value::as_str)?.to_owned();
            let index = ann.get("index").and_then(Value::as_u64);
            (
                format!("file|{file_id}|{index:?}|{span:?}"),
                RawCitation::File {
                    provider_file_id: file_id,
                    span,
                },
            )
        }
        _ => return None,
    };
    st.citations_seen
        .insert(key)
        .then_some(LlmEvent::Citation(raw))
}

/// Citations from the final `response.output[].content[].annotations`.
fn final_citations(st: &mut ParseState, response: &Value) -> Vec<LlmEvent> {
    let mut out = Vec::new();
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            let Some(anns) = part.get("annotations").and_then(Value::as_array) else {
                continue;
            };
            for ann in anns {
                if let Some(ev) = citation(st, ann, text) {
                    out.push(ev);
                }
            }
        }
    }
    out
}

/// `logs` outputs of a `code_interpreter_call` item, joined and capped.
fn code_output(item: &Value) -> String {
    let logs: Vec<&str> = item
        .get("outputs")
        .and_then(Value::as_array)
        .map(|outs| {
            outs.iter()
                .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                .filter_map(|o| o.get("logs").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let joined = logs.join("\n");
    if joined.chars().count() > CODE_OUTPUT_MAX_CHARS {
        let mut capped: String = joined.chars().take(CODE_OUTPUT_MAX_CHARS).collect();
        capped.push_str(TRUNCATED_SUFFIX);
        capped
    } else {
        joined
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn terminal(st: &mut ParseState, name: &str, data: &Value) -> Vec<LlmEvent> {
    let response = data.get("response").unwrap_or(&Value::Null);
    if let Some(id) = str_field(response, "id") {
        st.response_id = Some(id.to_owned());
    }
    let usage = parse_usage(response.get("usage"));
    let mut out = Vec::new();
    let incomplete_reason = if name == "response.incomplete" {
        Some(
            response
                .get("incomplete_details")
                .and_then(|d| str_field(d, "reason"))
                .unwrap_or("unknown")
                .to_owned(),
        )
    } else {
        out = final_citations(st, response);
        None
    };
    out.push(LlmEvent::Completed {
        usage,
        response_id: st.response_id.clone(),
        incomplete_reason,
    });
    out
}

fn translate(st: &mut ParseState, name: &str, data: &Value) -> Vec<LlmEvent> {
    match name {
        "response.created" | "response.in_progress" => {
            if let Some(id) = data.get("response").and_then(|r| str_field(r, "id")) {
                st.response_id = Some(id.to_owned());
            }
            Vec::new()
        }
        "response.output_text.delta" => {
            let Some(delta) = str_field(data, "delta") else {
                return Vec::new();
            };
            st.part_texts
                .entry(part_key(data))
                .or_default()
                .push_str(delta);
            if delta.is_empty() {
                Vec::new()
            } else {
                vec![LlmEvent::TextDelta(delta.to_owned())]
            }
        }
        "response.output_text.done" => {
            if let Some(text) = str_field(data, "text") {
                st.part_texts.insert(part_key(data), text.to_owned());
            }
            Vec::new()
        }
        "response.output_text.annotation.added" => {
            let text = st
                .part_texts
                .get(&part_key(data))
                .cloned()
                .unwrap_or_default();
            data.get("annotation")
                .and_then(|a| citation(st, a, &text))
                .into_iter()
                .collect()
        }
        "response.file_search_call.searching" => tool_start("file_search"),
        "response.file_search_call.completed" => {
            let n = data
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            vec![LlmEvent::ToolDone {
                name: "file_search".to_owned(),
                details: json!({"files_searched": n}),
            }]
        }
        "response.web_search_call.searching" => tool_start("web_search"),
        "response.web_search_call.completed" => vec![LlmEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({}),
        }],
        "response.code_interpreter_call.in_progress" => tool_start("code_interpreter"),
        "response.output_item.done" => match data.get("item") {
            Some(item) if str_field(item, "type") == Some("code_interpreter_call") => {
                vec![LlmEvent::ToolDone {
                    name: "code_interpreter".to_owned(),
                    details: json!({"output": code_output(item)}),
                }]
            }
            // A function tool call: no client tool event (DESIGN §3.3 "event: tool").
            Some(item) if str_field(item, "type") == Some("function_call") => {
                vec![LlmEvent::FunctionCall(FunctionCall {
                    call_id: str_field(item, "call_id")
                        .or_else(|| str_field(item, "id"))
                        .unwrap_or_default()
                        .to_owned(),
                    name: str_field(item, "name").unwrap_or_default().to_owned(),
                    arguments: str_field(item, "arguments").unwrap_or("{}").to_owned(),
                })]
            }
            _ => Vec::new(),
        },
        "response.completed" | "response.incomplete" => terminal(st, name, data),
        "response.failed" => {
            let usage = data
                .get("response")
                .and_then(|r| parse_usage(r.get("usage")));
            vec![provider_failed(
                failed_message(data).unwrap_or_else(|| "provider response failed".to_owned()),
                usage,
            )]
        }
        "error" => {
            let usage = data
                .get("response")
                .and_then(|r| parse_usage(r.get("usage")));
            let message = failed_message(data)
                .or_else(|| error_message(data))
                .unwrap_or_else(|| data.to_string());
            vec![provider_failed(message, usage)]
        }
        _ => Vec::new(),
    }
}

impl ProviderAdapter for OpenAiResponsesAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        build(req)
    }

    fn parse_event(&self, st: &mut ParseState, event: &str, data: &str) -> Vec<LlmEvent> {
        let parsed = serde_json::from_str::<Value>(data).ok();
        let name = if event.is_empty() || event == "message" {
            parsed.as_ref().and_then(|v| str_field(v, "type"))
        } else {
            Some(event)
        };
        match (name, parsed.as_ref()) {
            (Some(name), Some(v)) if v.is_object() => translate(st, name, v),
            // A JSON string `error` payload is the message itself.
            (Some("error"), Some(Value::String(s))) => vec![provider_failed(s.clone(), None)],
            // An unparseable (or non-object) `error` event: its data is the message.
            (Some("error"), _) => vec![provider_failed(data.to_owned(), None)],
            _ => Vec::new(),
        }
    }

    fn parse_completion(&self, body: &[u8]) -> Result<CompletionResult, LlmError> {
        let v: Value = serde_json::from_slice(body).map_err(|_| LlmError::Provider {
            message: "invalid provider response".to_owned(),
        })?;
        if str_field(&v, "status") == Some("failed") || v.get("error").is_some_and(|e| !e.is_null())
        {
            return Err(LlmError::Provider {
                message: failed_message(&json!({"response": v}))
                    .unwrap_or_else(|| "provider response failed".to_owned()),
            });
        }
        let mut text = String::new();
        for item in v
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if str_field(item, "type") != Some("message") {
                continue;
            }
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if str_field(part, "type") == Some("output_text")
                    && let Some(t) = str_field(part, "text")
                {
                    text.push_str(t);
                }
            }
        }
        if text.is_empty()
            && let Some(t) = str_field(&v, "output_text")
        {
            t.clone_into(&mut text);
        }
        Ok(CompletionResult {
            text,
            usage: parse_usage(v.get("usage")),
        })
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod openai_responses_tests;
