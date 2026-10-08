use super::*;

fn req() -> ChatRequest {
    ChatRequest {
        model: "gpt-x".to_owned(),
        instructions: "be nice".to_owned(),
        input: vec![
            InputMessage {
                role: Role::User,
                text: "q1".to_owned(),
                image_file_ids: Vec::new(),
            },
            InputMessage {
                role: Role::Assistant,
                text: "a1".to_owned(),
                image_file_ids: Vec::new(),
            },
            InputMessage {
                role: Role::User,
                text: "look".to_owned(),
                image_file_ids: vec!["file-img".to_owned()],
            },
        ],
        max_output_tokens: 1000,
        tools: Vec::new(),
        include_code_interpreter_outputs: false,
        max_tool_calls: Some(2),
        user: "u".to_owned(),
        metadata: Some(json!({"request_type": "chat"})),
        api_params: ModelApiParams::default(),
        stream: true,
        extra_input: Vec::new(),
    }
}

#[test]
fn body_shape() {
    let b = build_body(&req());
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["instructions"], "be nice");
    assert_eq!(b["stream"], true);
    assert_eq!(b["store"], false);
    assert_eq!(b["max_output_tokens"], 1000);
    assert_eq!(b["user"], "u");
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(
        b["input"][0],
        json!({"role": "user", "content": [{"type": "input_text", "text": "q1"}]})
    );
    assert_eq!(
        b["input"][1],
        json!({"role": "assistant", "content": [{"type": "output_text", "text": "a1"}]})
    );
    assert_eq!(
        b["input"][2]["content"],
        json!([{"type": "input_text", "text": "look"}, {"type": "input_image", "file_id": "file-img"}])
    );
    // no tools, but the built-in tool call bound is always sent
    assert!(b.get("tools").is_none());
    assert_eq!(b["max_tool_calls"], 2);
    assert!(b.get("include").is_none());
    assert!(b.get("temperature").is_none());
}

#[test]
fn body_tools_params_and_extra_body() {
    let mut r = req();
    r.tools = vec![json!({"type": "web_search"})];
    r.include_code_interpreter_outputs = true;
    r.api_params.temperature = Some(0.5);
    r.api_params.reasoning_effort = Some("low".to_owned());
    let mut extra = Map::new();
    extra.insert("model".to_owned(), json!("override"));
    extra.insert("store".to_owned(), json!(true));
    extra.insert("custom_flag".to_owned(), json!(42));
    r.api_params.extra_body = Some(extra);
    r.extra_input = vec![json!({"type": "function_call_output", "call_id": "c", "output": "x"})];
    let b = build_body(&r);
    assert_eq!(
        b["input"].as_array().unwrap().last().unwrap()["type"],
        "function_call_output"
    );
    assert_eq!(b["tools"], json!([{"type": "web_search"}]));
    assert_eq!(b["max_tool_calls"], 2);
    assert_eq!(b["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(b["temperature"], 0.5);
    assert_eq!(b["reasoning"], json!({"effort": "low"}));
    assert_eq!(b["custom_flag"], 42);
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["store"], false);
}

#[test]
fn usage_parsing() {
    let u = parse_usage(&json!({
        "input_tokens": 10, "output_tokens": 5,
        "input_tokens_details": {"cached_tokens": 3},
        "output_tokens_details": {"reasoning_tokens": 2}
    }))
    .unwrap();
    assert_eq!(
        (
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.reasoning_tokens
        ),
        (10, 5, 3, 2)
    );
    let u = parse_usage(&json!({"prompt_tokens": 7, "completion_tokens": 1})).unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (7, 1));
    assert!(parse_usage(&json!(null)).is_none());
}

#[test]
fn deltas_and_completion() {
    let mut p = ResponsesParser::default();
    let ev = p.on_event(
        Some("response.output_text.delta"),
        r#"{"delta":"Hel","output_index":0,"content_index":0}"#,
    );
    assert!(matches!(&ev[..], [ProviderEvent::TextDelta(t)] if t == "Hel"));
    // event name falls back to data.type
    let ev = p.on_event(
        None,
        r#"{"type":"response.output_text.delta","delta":"lo"}"#,
    );
    assert!(matches!(&ev[..], [ProviderEvent::TextDelta(t)] if t == "lo"));
    let ev = p.on_event(
        Some("message"),
        r#"{"type":"response.output_text.delta","delta":""}"#,
    );
    assert!(ev.is_empty());
    let ev = p.on_event(
        Some("response.completed"),
        r#"{"response":{"id":"resp_1","usage":{"input_tokens":3,"output_tokens":2},"output":[{"type":"message","content":[{"type":"output_text","text":"Hello","annotations":[]}]}]}}"#,
    );
    match &ev[..] {
        [ProviderEvent::Completed(c)] => {
            assert_eq!(c.response_id.as_deref(), Some("resp_1"));
            assert_eq!(c.usage.unwrap().input_tokens, 3);
            assert!(c.incomplete_reason.is_none());
            assert_eq!(
                c.parts.iter().map(|p| p.text.as_str()).collect::<String>(),
                "Hello"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(p.saw_terminal);
}

#[test]
fn incomplete_has_reason() {
    let mut p = ResponsesParser::default();
    let ev = p.on_event(
        Some("response.incomplete"),
        r#"{"response":{"id":"r","incomplete_details":{"reason":"max_output_tokens"},"output":[]}}"#,
    );
    match &ev[..] {
        [ProviderEvent::Completed(c)] => {
            assert_eq!(c.incomplete_reason.as_deref(), Some("max_output_tokens"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn tool_events() {
    let mut p = ResponsesParser::default();
    let ev = p.on_event(Some("response.web_search_call.searching"), "{}");
    assert!(matches!(
        &ev[..],
        [ProviderEvent::ToolStart {
            name: "web_search",
            ..
        }]
    ));
    let ev = p.on_event(
        Some("response.file_search_call.completed"),
        r#"{"results":[{},{},{}]}"#,
    );
    match &ev[..] {
        [
            ProviderEvent::ToolDone {
                name: "file_search",
                details,
            },
        ] => assert_eq!(details["files_searched"], 3),
        other => panic!("unexpected {other:?}"),
    }
    let ev = p.on_event(Some("response.code_interpreter_call.in_progress"), "{}");
    assert!(matches!(
        &ev[..],
        [ProviderEvent::ToolStart {
            name: "code_interpreter",
            ..
        }]
    ));
    let ev = p.on_event(
        Some("response.output_item.done"),
        r#"{"item":{"type":"code_interpreter_call","outputs":[{"type":"logs","logs":"a"},{"type":"image"},{"type":"logs","logs":"b"}]}}"#,
    );
    match &ev[..] {
        [
            ProviderEvent::ToolDone {
                name: "code_interpreter",
                details,
            },
        ] => assert_eq!(details["output"], "a\nb"),
        other => panic!("unexpected {other:?}"),
    }
    let ev = p.on_event(
        Some("response.output_item.done"),
        r#"{"item":{"type":"function_call","name":"search_knowledge"}}"#,
    );
    assert!(
        matches!(&ev[..], [ProviderEvent::FunctionCall { name, .. }] if name == "search_knowledge")
    );
    let ev = p.on_event(
        Some("response.output_item.done"),
        r#"{"item":{"type":"function_call","call_id":"call_9","name":"search_knowledge","arguments":"{\"query\":\"q\"}"}}"#,
    );
    assert!(
        matches!(&ev[..], [ProviderEvent::FunctionCall { call_id, arguments, .. }] if call_id == "call_9" && arguments.contains("query"))
    );
    assert!(!p.saw_terminal);
}

#[test]
fn code_output_is_capped() {
    let long = "x".repeat(CODE_OUTPUT_CAP + 10);
    let item =
        json!({"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": long}]});
    let out = code_output(&item);
    assert!(out.ends_with("...[truncated]"));
    assert_eq!(
        out.chars().count(),
        CODE_OUTPUT_CAP + "...[truncated]".len()
    );
}

#[test]
fn failures() {
    let mut p = ResponsesParser::default();
    let ev = p.on_event(
        Some("response.failed"),
        r#"{"response":{"error":{"code":"server_error","message":"bad"},"usage":{"input_tokens":1,"output_tokens":0}}}"#,
    );
    match &ev[..] {
        [
            ProviderEvent::Failed {
                code,
                message,
                usage,
            },
        ] => {
            assert_eq!(code.as_deref(), Some("server_error"));
            assert_eq!(message, "bad");
            assert_eq!(usage.unwrap().input_tokens, 1);
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut p = ResponsesParser::default();
    let ev = p.on_event(Some("error"), r#"{"code":"rate","message":"slow"}"#);
    assert!(matches!(&ev[..], [ProviderEvent::Failed { message, .. }] if message == "slow"));
    let ev = p.on_event(Some("error"), "not json");
    assert!(matches!(&ev[..], [ProviderEvent::Failed { message, .. }] if message == "not json"));
    assert!(p.saw_terminal);
}

#[test]
fn annotations_from_stream_are_kept() {
    let mut p = ResponsesParser::default();
    p.on_event(
        Some("response.output_text.delta"),
        r#"{"delta":"Hi","output_index":0,"content_index":0}"#,
    );
    p.on_event(
        Some("response.output_text.annotation.added"),
        r#"{"output_index":0,"content_index":0,"annotation":{"type":"url_citation","url":"https://x"}}"#,
    );
    let ev = p.on_event(
        Some("response.completed"),
        r#"{"response":{"id":"r","output":[]}}"#,
    );
    match &ev[..] {
        [ProviderEvent::Completed(c)] => {
            assert_eq!(c.parts.len(), 1);
            assert_eq!(c.parts[0].annotations.len(), 1);
            assert_eq!(c.parts[0].text, "Hi");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn non_streaming_text() {
    assert_eq!(response_text(&json!({"output_text": "direct"})), "direct");
    let v = json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "a"}, {"type": "output_text", "text": "b"}]}]});
    assert_eq!(response_text(&v), "ab");
}
