//! OpenAI / Azure OpenAI Responses API adapter (`openai_responses`).

use serde_json::{Map, Value, json};

use super::sse::SseFrame;
use super::{ChatRequest, ProviderErrorKind, ProviderEvent, ProviderUsage, RawCitation, apply_api_params};

/// Code interpreter output cap in `tool` done details.
pub const CODE_OUTPUT_CAP: usize = 8192;

/// Builds the Responses API request body.
#[must_use]
pub fn build_body(req: &ChatRequest, with_metadata: bool, with_max_tool_calls: bool, with_tools: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(req.model.clone()));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), Value::String(req.instructions.clone()));
    }
    let input: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            if m.role == "assistant" {
                json!({"role": "assistant", "content": [{"type": "output_text", "text": m.text}]})
            } else {
                let mut content = vec![json!({"type": "input_text", "text": m.text})];
                for f in &m.image_file_ids {
                    content.push(json!({"type": "input_image", "file_id": f}));
                }
                json!({"role": m.role, "content": content})
            }
        })
        .collect();
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), Value::Bool(req.stream));
    body.insert("max_output_tokens".into(), Value::from(req.max_output_tokens));
    body.insert("user".into(), Value::String(req.user.clone()));
    if with_metadata {
        body.insert("metadata".into(), Value::Object(req.metadata.clone()));
    }
    if with_tools && !req.tools.is_empty() {
        let mut tools = Vec::new();
        if let Some(fs) = &req.tools.file_search {
            tools.push(json!({"type": "file_search", "vector_store_ids": [fs.vector_store_id], "max_num_results": fs.max_num_results}));
        }
        if let Some(size) = &req.tools.web_search {
            tools.push(json!({"type": "web_search", "search_context_size": size}));
        }
        if let Some(files) = &req.tools.code_interpreter {
            tools.push(json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": files}}));
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
        body.insert("tools".into(), Value::Array(tools));
        if with_max_tool_calls && let Some(n) = req.tools.max_tool_calls {
            body.insert("max_tool_calls".into(), Value::from(n));
        }
    }
    if !req.api_params.stop.is_empty() {
        // The Responses API has no `stop`; keep it out of the body.
    }
    if let Some(effort) = &req.api_params.reasoning_effort {
        body.insert("reasoning".into(), json!({"effort": effort}));
    }
    apply_api_params(&mut body, &req.api_params, true);
    Value::Object(body)
}

/// Parses Responses `usage`.
#[must_use]
pub fn parse_usage(v: &Value) -> Option<ProviderUsage> {
    let u = v.as_object()?;
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(ProviderUsage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_input_tokens: u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        cache_write_input_tokens: 0,
        reasoning_tokens: u
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

/// Parses one annotation object.
#[must_use]
pub fn parse_annotation(a: &Value) -> Option<RawCitation> {
    let t = a.get("type").and_then(Value::as_str)?;
    let idx = |k: &str| a.get(k).and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
    match t {
        "url_citation" => Some(RawCitation::Url {
            url: a.get("url").and_then(Value::as_str)?.to_owned(),
            title: a.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(),
            start: idx("start_index"),
            end: idx("end_index"),
        }),
        "file_citation" | "file_path" | "container_file_citation" => Some(RawCitation::File {
            file_id: a.get("file_id").and_then(Value::as_str)?.to_owned(),
            filename: a.get("filename").and_then(Value::as_str).map(str::to_owned),
        }),
        _ => None,
    }
}

fn error_message(v: &Value) -> String {
    let err = v
        .get("response")
        .and_then(|r| r.get("error"))
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()));
    if let Some(e) = err {
        if let Some(m) = e.get("message").and_then(Value::as_str) {
            return m.to_owned();
        }
        if let Some(s) = e.as_str() {
            return s.to_owned();
        }
    }
    if let Some(m) = v.get("message").and_then(Value::as_str) {
        return m.to_owned();
    }
    "Provider returned an error".to_owned()
}

/// Stateful decoder of the Responses SSE stream.
#[derive(Debug, Default)]
pub struct ResponsesDecoder {
    response_id: Option<String>,
    seen_annotations: Vec<RawCitation>,
    terminal: bool,
}

impl ResponsesDecoder {
    fn push_citation(&mut self, out: &mut Vec<ProviderEvent>, c: RawCitation) {
        if !self.seen_annotations.contains(&c) {
            self.seen_annotations.push(c.clone());
            out.push(ProviderEvent::Citation(c));
        }
    }

    fn collect_output_annotations(&mut self, response: &Value, out: &mut Vec<ProviderEvent>) {
        let Some(items) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        for item in items {
            let Some(content) = item.get("content").and_then(Value::as_array) else {
                continue;
            };
            for part in content {
                if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                    for a in anns {
                        if let Some(c) = parse_annotation(a) {
                            self.push_citation(out, c);
                        }
                    }
                }
            }
        }
    }

    /// Whether a terminal event was seen.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Decodes one SSE frame.
    #[allow(clippy::too_many_lines)]
    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if frame.data.trim() == "[DONE]" {
            return out;
        }
        let data: Value = serde_json::from_str(&frame.data).unwrap_or(Value::Null);
        let name = match frame.event.as_deref() {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => data.get("type").and_then(Value::as_str).unwrap_or_default().to_owned(),
        };
        match name.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(id) = data.get("response").and_then(|r| r.get("id")).and_then(Value::as_str)
                    && self.response_id.is_none()
                {
                    self.response_id = Some(id.to_owned());
                    out.push(ProviderEvent::ResponseId(id.to_owned()));
                }
            }
            "response.output_text.delta" => {
                if let Some(d) = data.get("delta").and_then(Value::as_str)
                    && !d.is_empty()
                {
                    out.push(ProviderEvent::TextDelta(d.to_owned()));
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(c) = data.get("annotation").and_then(parse_annotation) {
                    self.push_citation(&mut out, c);
                }
            }
            "response.file_search_call.searching" => out.push(ProviderEvent::ToolStart {
                name: "file_search".into(),
                details: json!({}),
            }),
            "response.file_search_call.completed" => {
                let n = data.get("results").and_then(Value::as_array).map_or(0, Vec::len);
                out.push(ProviderEvent::ToolDone {
                    name: "file_search".into(),
                    details: json!({"files_searched": n}),
                });
            }
            "response.web_search_call.searching" => out.push(ProviderEvent::ToolStart {
                name: "web_search".into(),
                details: json!({}),
            }),
            "response.web_search_call.completed" => out.push(ProviderEvent::ToolDone {
                name: "web_search".into(),
                details: json!({}),
            }),
            "response.code_interpreter_call.in_progress" => out.push(ProviderEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({}),
            }),
            "response.output_item.done" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("code_interpreter_call") {
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
                    let mut output = logs.join("\n");
                    if output.chars().count() > CODE_OUTPUT_CAP {
                        output = output.chars().take(CODE_OUTPUT_CAP).collect::<String>() + "...[truncated]";
                    }
                    out.push(ProviderEvent::ToolDone {
                        name: "code_interpreter".into(),
                        details: json!({"output": output}),
                    });
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = data.get("response").cloned().unwrap_or(Value::Null);
                self.collect_output_annotations(&response, &mut out);
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| self.response_id.clone());
                let reason = (name == "response.incomplete").then(|| {
                    response
                        .get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                self.terminal = true;
                out.push(ProviderEvent::Completed {
                    response_id: id,
                    usage: response.get("usage").and_then(parse_usage),
                    incomplete_reason: reason,
                });
            }
            "response.failed" => {
                self.terminal = true;
                out.push(ProviderEvent::Failed {
                    kind: ProviderErrorKind::ProviderError,
                    message: error_message(&data),
                    usage: data.get("response").and_then(|r| r.get("usage")).and_then(parse_usage),
                });
            }
            "error" => {
                self.terminal = true;
                let message = if data.is_null() {
                    frame.data.clone()
                } else {
                    error_message(&data)
                };
                out.push(ProviderEvent::Failed {
                    kind: ProviderErrorKind::ProviderError,
                    message,
                    usage: None,
                });
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod tests;
