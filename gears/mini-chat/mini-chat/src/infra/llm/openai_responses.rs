//! OpenAI / Azure OpenAI Responses API adapter (also used by vLLM Responses,
//! which drops tools and metadata and maps `<think>` text to reasoning).

use serde_json::{Map, Value, json};

use super::{
    ContentPart, LlmRequest, ProviderEvent, RawCitation, ToolSpec, TranslateState,
    apply_api_params, codes, parse_usage,
};

const CODE_OUTPUT_CAP: usize = 8192;

fn input_item(msg: &super::InputMessage) -> Value {
    let only_text = msg.parts.len() == 1 && matches!(msg.parts[0], ContentPart::Text(_));
    if only_text {
        return json!({ "role": msg.role, "content": msg.joined_text() });
    }
    let text_type = if msg.role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    let content: Vec<Value> = msg
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => json!({ "type": text_type, "text": t }),
            ContentPart::Image(file_id) => json!({ "type": "input_image", "file_id": file_id }),
        })
        .collect();
    json!({ "role": msg.role, "content": content })
}

fn tool_json(tool: &ToolSpec) -> Value {
    match tool {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({ "type": "web_search", "search_context_size": search_context_size }),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": { "type": "auto", "file_ids": file_ids },
        }),
    }
}

/// Request body. `openai = false` renders the vLLM variant (no tools, no
/// metadata).
#[must_use]
pub fn build_body(req: &LlmRequest, openai: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.provider_model_id));
    body.insert(
        "input".into(),
        Value::Array(req.input.iter().map(input_item).collect()),
    );
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    body.insert("user".into(), json!(req.user));
    if openai {
        body.insert("metadata".into(), Value::Object(req.metadata.clone()));
        if !req.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(req.tools.iter().map(tool_json).collect()),
            );
            body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
            if req
                .tools
                .iter()
                .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
            {
                body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
            }
        }
    }
    apply_api_params(&mut body, &req.api_params, false);
    Value::Object(body)
}

fn span_of(a: &Value) -> Option<(u64, u64)> {
    match (
        a.get("start_index").and_then(Value::as_u64),
        a.get("end_index").and_then(Value::as_u64),
    ) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    }
}

fn char_slice(text: &str, start: u64, end: u64) -> String {
    let s = usize::try_from(start).unwrap_or(usize::MAX);
    let e = usize::try_from(end).unwrap_or(usize::MAX);
    if s >= e {
        return String::new();
    }
    let count = text.chars().count();
    if e > count {
        return String::new();
    }
    text.chars().skip(s).take(e - s).collect()
}

/// Maps one provider annotation to a raw citation.
#[must_use]
pub fn map_annotation(a: &Value, part_text: &str) -> Option<RawCitation> {
    match a.get("type").and_then(Value::as_str)? {
        "url_citation" => {
            let span = span_of(a);
            let snippet = a
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| span.map(|(s, e)| char_slice(part_text, s, e)))
                .unwrap_or_default();
            Some(RawCitation::Web {
                url: a.get("url").and_then(Value::as_str)?.to_owned(),
                title: a
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                snippet,
                span,
            })
        }
        "file_citation" => Some(RawCitation::File {
            file_id: a.get("file_id").and_then(Value::as_str)?.to_owned(),
            filename: a
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            span: span_of(a),
        }),
        _ => None,
    }
}

fn citations_from_response(resp: &Value) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    for item in resp
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            for a in part
                .get("annotations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(c) = map_annotation(a, text) {
                    out.push(ProviderEvent::Citation(c));
                }
            }
        }
    }
    out
}

fn split_think(state: &mut TranslateState, delta: &str) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    let mut rest = delta;
    while !rest.is_empty() {
        if state.think_open {
            if let Some(pos) = rest.find("</think>") {
                if pos > 0 {
                    out.push(ProviderEvent::ReasoningDelta(rest[..pos].to_owned()));
                }
                state.think_open = false;
                rest = &rest[pos + "</think>".len()..];
            } else {
                out.push(ProviderEvent::ReasoningDelta(rest.to_owned()));
                break;
            }
        } else if let Some(pos) = rest.find("<think>") {
            if pos > 0 {
                out.push(ProviderEvent::TextDelta(rest[..pos].to_owned()));
            }
            state.think_open = true;
            rest = &rest[pos + "<think>".len()..];
        } else {
            out.push(ProviderEvent::TextDelta(rest.to_owned()));
            break;
        }
    }
    out
}

fn failure_from(v: &Value) -> (String, Option<mini_chat_sdk::UsageTokens>) {
    let resp = v.get("response");
    let err = resp
        .and_then(|r| r.get("error"))
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error"))
        .unwrap_or(v);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| err.as_str().map(str::to_owned))
        .unwrap_or_else(|| "Provider returned an error".to_owned());
    let usage = resp.and_then(|r| r.get("usage")).and_then(parse_usage);
    (message, usage)
}

fn code_output(item: &Value) -> String {
    let logs: Vec<&str> = item
        .get("outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
        .filter_map(|o| o.get("logs").and_then(Value::as_str))
        .collect();
    let joined = logs.join("\n");
    if joined.chars().count() > CODE_OUTPUT_CAP {
        let mut s: String = joined.chars().take(CODE_OUTPUT_CAP).collect();
        s.push_str("...[truncated]");
        s
    } else {
        joined
    }
}

/// Translates one Responses API SSE frame.
pub fn translate(
    state: &mut TranslateState,
    event: Option<&str>,
    data: &str,
    vllm: bool,
) -> Vec<ProviderEvent> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Vec::new();
    }
    let parsed: Option<Value> = serde_json::from_str(data).ok();
    let name = event
        .filter(|e| !e.is_empty() && *e != "message")
        .map(str::to_owned)
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let Some(v) = parsed else {
        if name == "error" {
            return vec![ProviderEvent::Failed {
                code: codes::PROVIDER_ERROR,
                message: data.to_owned(),
                usage: None,
            }];
        }
        return Vec::new();
    };

    match name.as_str() {
        "response.output_text.delta" => {
            let delta = v.get("delta").and_then(Value::as_str).unwrap_or_default();
            if delta.is_empty() {
                return Vec::new();
            }
            state.part_text.push_str(delta);
            if vllm {
                split_think(state, delta)
            } else {
                vec![ProviderEvent::TextDelta(delta.to_owned())]
            }
        }
        "response.reasoning_text.delta" if vllm => {
            let delta = v.get("delta").and_then(Value::as_str).unwrap_or_default();
            vec![ProviderEvent::ReasoningDelta(delta.to_owned())]
        }
        "response.content_part.added" => {
            state.part_text.clear();
            Vec::new()
        }
        "response.output_text.annotation.added" => {
            state.saw_annotation_events = true;
            v.get("annotation")
                .and_then(|a| map_annotation(a, &state.part_text))
                .map(ProviderEvent::Citation)
                .into_iter()
                .collect()
        }
        "response.file_search_call.searching" => vec![ProviderEvent::ToolStart {
            name: "file_search".into(),
            details: json!({}),
        }],
        "response.file_search_call.completed" => {
            let n = v
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            vec![ProviderEvent::ToolDone {
                name: "file_search".into(),
                details: json!({ "files_searched": n }),
            }]
        }
        "response.web_search_call.searching" => vec![ProviderEvent::ToolStart {
            name: "web_search".into(),
            details: json!({}),
        }],
        "response.web_search_call.completed" => vec![ProviderEvent::ToolDone {
            name: "web_search".into(),
            details: json!({}),
        }],
        "response.code_interpreter_call.in_progress" => vec![ProviderEvent::ToolStart {
            name: "code_interpreter".into(),
            details: json!({}),
        }],
        "response.output_item.done" => {
            let item = v.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("code_interpreter_call") {
                vec![ProviderEvent::ToolDone {
                    name: "code_interpreter".into(),
                    details: json!({ "output": code_output(item) }),
                }]
            } else {
                Vec::new()
            }
        }
        "response.completed" | "response.incomplete" => {
            let resp = v.get("response").unwrap_or(&Value::Null);
            let mut out = Vec::new();
            if !state.saw_annotation_events && name == "response.completed" {
                out.extend(citations_from_response(resp));
            }
            let incomplete_reason = (name == "response.incomplete").then(|| {
                resp.get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("other")
                    .to_owned()
            });
            out.push(ProviderEvent::Completed {
                usage: resp.get("usage").and_then(parse_usage),
                response_id: resp.get("id").and_then(Value::as_str).map(str::to_owned),
                incomplete_reason,
            });
            out
        }
        "response.failed" | "error" => {
            let (message, usage) = failure_from(&v);
            vec![ProviderEvent::Failed {
                code: codes::PROVIDER_ERROR,
                message,
                usage,
            }]
        }
        _ => Vec::new(),
    }
}

/// Text and usage of a non-streaming response.
#[must_use]
pub fn parse_completion(body: &Value) -> (String, Option<mini_chat_sdk::UsageTokens>) {
    let mut text = body
        .get("output_text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    if text.is_empty() {
        for item in body
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) == Some("output_text")
                    && let Some(t) = part.get("text").and_then(Value::as_str)
                {
                    text.push_str(t);
                }
            }
        }
    }
    (text, body.get("usage").and_then(parse_usage))
}
