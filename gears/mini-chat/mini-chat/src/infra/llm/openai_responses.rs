//! OpenAI / Azure OpenAI Responses API adapter (`openai_responses`).

use serde_json::{Map, Value, json};

use super::{
    Adapter, ChatItem, Completion, ItemRole, LlmEvent, LlmFailure, LlmRequest, ParseState,
    RawCitation, Usage, error_message, parse_responses_usage,
};

/// Keys the request controls; `extra_body` cannot override them.
pub const RESERVED_KEYS: &[&str] = &[
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

pub const CODE_OUTPUT_CAP: usize = 8192;

pub struct OpenAiResponses;

pub(crate) fn input_items(items: &[ChatItem]) -> Vec<Value> {
    items
        .iter()
        .map(|it| {
            let role = match it.role {
                ItemRole::User => "user",
                ItemRole::Assistant => "assistant",
            };
            if it.images.is_empty() {
                json!({"role": role, "content": it.text})
            } else {
                let mut content = vec![json!({"type": "input_text", "text": it.text})];
                for f in &it.images {
                    content.push(json!({"type": "input_image", "file_id": f}));
                }
                json!({"role": role, "content": content})
            }
        })
        .collect()
}

pub(crate) fn apply_api_params(body: &mut Map<String, Value>, req: &LlmRequest) {
    let p = &req.api_params;
    if let Some(v) = p.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if let Some(v) = p.frequency_penalty {
        body.insert("frequency_penalty".into(), json!(v));
    }
    if let Some(v) = p.presence_penalty {
        body.insert("presence_penalty".into(), json!(v));
    }
    if let Some(effort) = &p.reasoning_effort {
        body.insert("reasoning".into(), json!({"effort": effort}));
    }
}

pub(crate) fn apply_extra_body(body: &mut Map<String, Value>, req: &LlmRequest) {
    if let Some(extra) = &req.api_params.extra_body {
        for (k, v) in extra {
            if RESERVED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "extra_body key ignored: controlled by the request");
                continue;
            }
            body.insert(k.clone(), v.clone());
        }
    }
}

pub(crate) fn tools_json(req: &LlmRequest) -> Vec<Value> {
    let mut tools = Vec::new();
    if let Some(fs) = &req.tools.file_search {
        tools.push(json!({
            "type": "file_search",
            "vector_store_ids": fs.vector_store_ids,
            "max_num_results": fs.max_num_results,
        }));
    }
    if let Some(size) = &req.tools.web_search {
        tools.push(json!({"type": "web_search", "search_context_size": size}));
    }
    if let Some(files) = &req.tools.code_interpreter {
        tools.push(json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": files},
        }));
    }
    if req.tools.knowledge {
        tools.push(json!({
            "type": "function",
            "name": super::KNOWLEDGE_TOOL,
            "description": super::KNOWLEDGE_DESCRIPTION,
            "parameters": super::knowledge_parameters(),
        }));
    }
    tools
}

fn truncate_output(s: &str) -> String {
    if s.chars().count() <= CODE_OUTPUT_CAP {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(CODE_OUTPUT_CAP).collect();
    out.push_str("...[truncated]");
    out
}

fn annotation_citation(ann: &Value, part_text: Option<&str>) -> Option<RawCitation> {
    let ty = ann.get("type").and_then(Value::as_str)?;
    let start = ann.get("start_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
    let end = ann.get("end_index").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
    match ty {
        "url_citation" => Some(RawCitation::Url {
            url: ann.get("url").and_then(Value::as_str).unwrap_or_default().to_owned(),
            title: ann.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(),
            start,
            end,
            text: ann.get("text").and_then(Value::as_str).map(str::to_owned),
            part_text: part_text.map(str::to_owned),
        }),
        "file_citation" | "container_file_citation" => Some(RawCitation::File {
            file_id: ann.get("file_id").and_then(Value::as_str)?.to_owned(),
            filename: ann.get("filename").and_then(Value::as_str).unwrap_or_default().to_owned(),
            start,
            end,
        }),
        _ => None,
    }
}

fn citation_key(c: &RawCitation) -> String {
    match c {
        RawCitation::Url { url, start, end, .. } => format!("u|{url}|{start:?}|{end:?}"),
        RawCitation::File { file_id, start, .. } => format!("f|{file_id}|{start:?}"),
    }
}

fn push_citation(state: &mut ParseState, out: &mut Vec<LlmEvent>, c: RawCitation) {
    let key = citation_key(&c);
    if !state.seen_citations.contains(&key) {
        state.seen_citations.push(key);
        out.push(LlmEvent::Citation(c));
    }
}

/// Citations and function calls from a final `response` object.
pub(crate) fn from_response_output(state: &mut ParseState, out: &mut Vec<LlmEvent>, response: &Value) {
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && let Some(parts) = item.get("content").and_then(Value::as_array)
        {
            for part in parts {
                let text = part.get("text").and_then(Value::as_str);
                if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                    for ann in anns {
                        if let Some(c) = annotation_citation(ann, text) {
                            push_citation(state, out, c);
                        }
                    }
                }
            }
        }
    }
}

/// Shared Responses-API event translation (also used by vLLM).
pub(crate) fn parse_responses_event(
    state: &mut ParseState,
    event: Option<&str>,
    data: &str,
    with_tools: bool,
) -> Vec<LlmEvent> {
    let mut out = Vec::new();
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => {
            if event == Some("error") {
                out.push(LlmEvent::Failed(LlmFailure::provider(data.to_owned())));
            }
            return out;
        }
    };
    let name = match event {
        Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
        _ => v.get("type").and_then(Value::as_str).unwrap_or_default().to_owned(),
    };
    match name.as_str() {
        "response.created" | "response.in_progress" => {
            if let Some(id) = v.get("response").and_then(|r| r.get("id")).and_then(Value::as_str) {
                state.response_id = Some(id.to_owned());
            }
        }
        "response.output_text.delta" => {
            if let Some(d) = v.get("delta").and_then(Value::as_str)
                && !d.is_empty()
            {
                state.text.push_str(d);
                out.push(LlmEvent::TextDelta(d.to_owned()));
            }
        }
        "response.output_text.annotation.added" => {
            if let Some(ann) = v.get("annotation") {
                let part_text = state.text.clone();
                if let Some(c) = annotation_citation(ann, Some(&part_text)) {
                    push_citation(state, &mut out, c);
                }
            }
        }
        "response.file_search_call.searching" if with_tools => out.push(LlmEvent::ToolStart {
            name: "file_search".into(),
            details: json!({}),
        }),
        "response.file_search_call.completed" if with_tools => {
            let n = v.get("results").and_then(Value::as_array).map_or(0, Vec::len);
            out.push(LlmEvent::ToolDone {
                name: "file_search".into(),
                details: json!({"files_searched": n}),
            });
        }
        "response.web_search_call.searching" if with_tools => out.push(LlmEvent::ToolStart {
            name: "web_search".into(),
            details: json!({}),
        }),
        "response.web_search_call.completed" if with_tools => out.push(LlmEvent::ToolDone {
            name: "web_search".into(),
            details: json!({}),
        }),
        "response.code_interpreter_call.in_progress" if with_tools => out.push(LlmEvent::ToolStart {
            name: "code_interpreter".into(),
            details: json!({}),
        }),
        "response.output_item.done" => {
            let item = v.get("item").cloned().unwrap_or(Value::Null);
            match item.get("type").and_then(Value::as_str) {
                Some("code_interpreter_call") if with_tools => {
                    let logs: Vec<String> = item
                        .get("outputs")
                        .and_then(Value::as_array)
                        .map(|outs| {
                            outs.iter()
                                .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                                .filter_map(|o| o.get("logs").and_then(Value::as_str).map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    out.push(LlmEvent::ToolDone {
                        name: "code_interpreter".into(),
                        details: json!({"output": truncate_output(&logs.join("\n"))}),
                    });
                }
                Some("function_call") => out.push(LlmEvent::FunctionCall {
                    name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    call_id: item.get("call_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    arguments: item.get("arguments").and_then(Value::as_str).unwrap_or_default().to_owned(),
                }),
                _ => {}
            }
        }
        "response.completed" | "response.incomplete" => {
            let resp = v.get("response").cloned().unwrap_or(Value::Null);
            from_response_output(state, &mut out, &resp);
            let usage = resp.get("usage").and_then(parse_responses_usage);
            let incomplete_reason = if name == "response.incomplete" {
                Some(
                    resp.get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned(),
                )
            } else {
                None
            };
            let response_id = resp
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| state.response_id.clone());
            out.push(LlmEvent::Completed(Completion {
                response_id,
                usage,
                incomplete_reason,
            }));
        }
        "response.failed" | "error" => {
            let usage = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .and_then(parse_responses_usage);
            let msg = error_message(&v).unwrap_or_else(|| "Provider returned an error".to_owned());
            let mut f = LlmFailure::provider(msg);
            f.usage = usage;
            out.push(LlmEvent::Failed(f));
        }
        _ => {}
    }
    out
}

/// Text of a non-streaming Responses API result.
pub(crate) fn complete_text(body: &Value) -> Result<(String, Option<Usage>), LlmFailure> {
    if let Some(msg) = body.get("error").filter(|e| !e.is_null()).and_then(|_| error_message(body)) {
        return Err(LlmFailure::provider(msg));
    }
    let mut text = String::new();
    if let Some(t) = body.get("output_text").and_then(Value::as_str) {
        text.push_str(t);
    } else if let Some(items) = body.get("output").and_then(Value::as_array) {
        for item in items {
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    Ok((text, body.get("usage").and_then(parse_responses_usage)))
}

impl Adapter for OpenAiResponses {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = Map::new();
        apply_extra_body(&mut body, req);
        body.insert("model".into(), json!(req.model));
        body.insert("stream".into(), json!(req.stream));
        body.insert("instructions".into(), json!(req.instructions));
        let mut input = input_items(&req.items);
        input.extend(req.extra_input.iter().cloned());
        body.insert("input".into(), Value::Array(input));
        body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
        let tools = tools_json(req);
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
        if req.tools.code_interpreter.is_some() {
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
        apply_api_params(&mut body, req);
        body.insert("store".into(), json!(false));
        body.insert("user".into(), json!(req.user));
        body.insert(
            "metadata".into(),
            json!({
                "tenant_id": req.metadata.tenant_id,
                "user_id": req.metadata.user_id,
                "chat_id": req.metadata.chat_id,
                "request_type": req.metadata.request_type,
                "feature": req.metadata.feature,
            }),
        );
        Value::Object(body)
    }

    fn parse_event(&self, state: &mut ParseState, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        parse_responses_event(state, event, data, true)
    }

    fn parse_complete(&self, body: &Value) -> Result<(String, Option<Usage>), LlmFailure> {
        complete_text(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::llm::{FileSearchTool, RequestMetadata, ToolsSpec};

    fn req() -> LlmRequest {
        LlmRequest {
            model: "gpt".into(),
            instructions: "sys".into(),
            items: vec![
                ChatItem { role: ItemRole::User, text: "hi".into(), images: vec![] },
                ChatItem { role: ItemRole::Assistant, text: "hello".into(), images: vec![] },
                ChatItem { role: ItemRole::User, text: "img?".into(), images: vec!["file-1".into()] },
            ],
            max_output_tokens: 100,
            tools: ToolsSpec {
                file_search: Some(FileSearchTool { vector_store_ids: vec!["vs_1".into()], max_num_results: 5 }),
                web_search: Some("low".into()),
                code_interpreter: Some(vec!["file-x".into()]),
                knowledge: false,
            },
            max_tool_calls: 2,
            api_params: mini_chat_sdk::ModelApiParams::default(),
            user: "u".into(),
            metadata: RequestMetadata {
                tenant_id: "t".into(),
                user_id: "u".into(),
                chat_id: "c".into(),
                request_type: "chat",
                feature: "file_search+web_search+code_interpreter".into(),
            },
            stream: true,
            extra_input: Vec::new(),
        }
    }

    #[test]
    fn body_shape() {
        let b = OpenAiResponses.build_body(&req());
        assert_eq!(b["stream"], true);
        assert_eq!(b["input"][0]["content"], "hi");
        assert_eq!(b["input"][2]["content"][1]["type"], "input_image");
        assert_eq!(b["tools"][0]["vector_store_ids"][0], "vs_1");
        assert_eq!(b["tools"][1]["type"], "web_search");
        assert_eq!(b["tools"][2]["container"]["file_ids"][0], "file-x");
        assert_eq!(b["include"][0], "code_interpreter_call.outputs");
        assert_eq!(b["max_tool_calls"], 2);
        assert_eq!(b["metadata"]["request_type"], "chat");
    }

    #[test]
    fn parses_stream() {
        let a = OpenAiResponses;
        let mut st = ParseState::default();
        let ev = a.parse_event(&mut st, Some("response.output_text.delta"), r#"{"delta":"Hel"}"#);
        assert_eq!(ev, vec![LlmEvent::TextDelta("Hel".into())]);
        let ev = a.parse_event(&mut st, None, r#"{"type":"response.web_search_call.searching"}"#);
        assert!(matches!(&ev[0], LlmEvent::ToolStart { name, .. } if name == "web_search"));
        let ev = a.parse_event(
            &mut st,
            Some("response.completed"),
            r#"{"response":{"id":"resp_1","usage":{"input_tokens":3,"output_tokens":4},"output":[{"type":"message","content":[{"type":"output_text","text":"Hello","annotations":[{"type":"url_citation","url":"https://e.x","title":"T","start_index":0,"end_index":3}]}]}]}}"#,
        );
        assert!(matches!(&ev[0], LlmEvent::Citation(RawCitation::Url { url, .. }) if url == "https://e.x"));
        match &ev[1] {
            LlmEvent::Completed(c) => {
                assert_eq!(c.response_id.as_deref(), Some("resp_1"));
                assert_eq!(c.usage.unwrap().output_tokens, 4);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_failure_sanitized() {
        let mut st = ParseState::default();
        let ev = OpenAiResponses.parse_event(
            &mut st,
            Some("response.failed"),
            r#"{"response":{"error":{"code":"x","message":"bad file-abcdefghijklmnop"}}}"#,
        );
        match &ev[0] {
            LlmEvent::Failed(f) => {
                assert_eq!(f.code, "provider_error");
                assert_eq!(f.message, "bad [provider_id]");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
