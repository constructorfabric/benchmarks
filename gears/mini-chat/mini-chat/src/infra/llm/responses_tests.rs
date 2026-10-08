use super::*;
use crate::infra::llm::sse::SseParser;
use mini_chat_sdk::{ApiParams, WebSearchContextSize};

fn req(tools: Vec<ToolSpec>) -> LlmRequest {
    let mut metadata = Map::new();
    metadata.insert("request_type".into(), json!("chat"));
    LlmRequest {
        model: "gpt-4.1".into(),
        instructions: "be nice".into(),
        input: vec![
            InputMessage::text(super::super::Role::User, "hi"),
            InputMessage::text(super::super::Role::Assistant, "hello"),
            InputMessage {
                role: super::super::Role::User,
                parts: vec![
                    ContentPart::Text("what is this".into()),
                    ContentPart::Image { file_id: "file-img".into(), secondary_file_id: None },
                ],
            },
        ],
        max_output_tokens: 1024,
        tools,
        max_tool_calls: 2,
        user: "u".repeat(64),
        metadata,
        api_params: ApiParams {
            temperature: Some(0.7),
            extra_body: Some(serde_json::from_value(json!({"seed": 3, "model": "evil"})).unwrap()),
            ..ApiParams::default()
        },
        stream: true,
    }
}

#[test]
fn request_shape_with_tools() {
    let body = build_request(
        &req(vec![
            ToolSpec::FileSearch { vector_store_ids: vec!["vs_1".into()], max_num_results: 5 },
            ToolSpec::WebSearch { context_size: WebSearchContextSize::Medium },
            ToolSpec::CodeInterpreter { file_ids: vec!["file-x".into()] },
        ]),
        Flavor::OpenAi,
    );
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["instructions"], "be nice");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_output_tokens"], 1024);
    assert_eq!(body["input"][0], json!({"role": "user", "content": "hi"}));
    assert_eq!(body["input"][1], json!({"role": "assistant", "content": "hello"}));
    assert_eq!(body["input"][2]["content"][1], json!({"type": "input_image", "file_id": "file-img"}));
    assert_eq!(body["tools"][0], json!({"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5}));
    assert_eq!(body["tools"][1], json!({"type": "web_search", "search_context_size": "medium"}));
    assert_eq!(body["tools"][2]["container"], json!({"type": "auto", "file_ids": ["file-x"]}));
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(body["metadata"]["request_type"], "chat");
    assert_eq!(body["temperature"], 0.7);
    assert!(body.get("top_p").is_none(), "unset params are not sent");
    assert_eq!(body["seed"], 3, "extra_body merged");
    assert_eq!(body["model"], "gpt-4.1", "reserved extra_body key ignored");
}

#[test]
fn request_without_tools_omits_tool_fields() {
    let body = build_request(&req(vec![]), Flavor::OpenAi);
    assert!(body.get("tools").is_none());
    assert!(body.get("max_tool_calls").is_none());
    assert!(body.get("include").is_none());
    let vllm = build_request(
        &req(vec![ToolSpec::WebSearch { context_size: WebSearchContextSize::Low }]),
        Flavor::Vllm,
    );
    assert!(vllm.get("tools").is_none(), "vLLM drops all tools");
    assert!(vllm.get("metadata").is_none());
}

fn run(frames: &str, flavor: Flavor) -> Vec<ProviderEvent> {
    let mut p = SseParser::default();
    let mut t = ResponsesTranslator::new(flavor);
    let mut out = Vec::new();
    for f in p.feed(frames.as_bytes()) {
        out.extend(t.on_frame(f.event.as_deref(), &f.data));
    }
    out
}

#[test]
fn translates_stream_with_tools_and_citations() {
    let frames = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        "event: response.web_search_call.searching\ndata: {}\n\n",
        "event: response.web_search_call.completed\ndata: {}\n\n",
        "event: response.file_search_call.searching\ndata: {}\n\n",
        "event: response.file_search_call.completed\ndata: {\"results\":[1,2]}\n\n",
        "event: response.code_interpreter_call.in_progress\ndata: {}\n\n",
        "event: response.output_item.done\ndata: {\"item\":{\"type\":\"code_interpreter_call\",\"outputs\":[{\"type\":\"logs\",\"logs\":\"a\"},{\"type\":\"logs\",\"logs\":\"b\"}]}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
        "event: response.output_text.delta\r\ndata: {\"delta\":\" world\"}\r\n\r\n",
        "event: response.completed\ndata: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens_details\":{\"reasoning_tokens\":1}},",
        "\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\",\"annotations\":[",
        "{\"type\":\"url_citation\",\"url\":\"https://e.com\",\"title\":\"E\",\"start_index\":0,\"end_index\":5},",
        "{\"type\":\"file_citation\",\"file_id\":\"file-1\",\"filename\":\"x\",\"index\":0}]}]}]}}\n\n",
    );
    let ev = run(frames, Flavor::OpenAi);
    assert_eq!(ev[0], ProviderEvent::ToolStart { name: "web_search".into(), details: json!({}) });
    assert_eq!(ev[1], ProviderEvent::ToolDone { name: "web_search".into(), details: json!({}) });
    assert_eq!(ev[3], ProviderEvent::ToolDone { name: "file_search".into(), details: json!({"files_searched": 2}) });
    assert_eq!(ev[4], ProviderEvent::ToolStart { name: "code_interpreter".into(), details: json!({}) });
    assert_eq!(ev[5], ProviderEvent::ToolDone { name: "code_interpreter".into(), details: json!({"output": "a\nb"}) });
    assert_eq!(ev[6], ProviderEvent::TextDelta("Hello".into()));
    assert_eq!(ev[7], ProviderEvent::TextDelta(" world".into()));
    match &ev[8] {
        ProviderEvent::Completed { usage, response_id, incomplete_reason, citations } => {
            let u = usage.unwrap();
            assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_input_tokens, u.reasoning_tokens), (10, 5, 2, 1));
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert!(incomplete_reason.is_none());
            assert_eq!(citations.len(), 2);
            assert_eq!(
                citations[0],
                RawCitation::Web { url: "https://e.com".into(), title: "E".into(), snippet: "Hello".into(), span: Some((0, 5)) }
            );
            assert_eq!(citations[1], RawCitation::File { file_id: "file-1".into() });
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn incomplete_failed_and_error_events() {
    let ev = run(
        "event: response.incomplete\ndata: {\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n",
        Flavor::OpenAi,
    );
    assert!(matches!(&ev[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens"));

    let ev = run(
        "event: response.failed\ndata: {\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"failed for file-abcdef0123456789\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        Flavor::OpenAi,
    );
    match &ev[0] {
        ProviderEvent::Failed(e) => {
            assert_eq!(e.code, ProviderErrorCode::ProviderError);
            assert_eq!(e.message, "failed for [provider_id]");
            assert_eq!(e.usage.unwrap().input_tokens, 3);
        }
        other => panic!("unexpected {other:?}"),
    }
    let ev = run("event: error\ndata: {\"code\":\"x\",\"message\":\"flat\"}\n\n", Flavor::OpenAi);
    assert!(matches!(&ev[0], ProviderEvent::Failed(e) if e.message == "flat"));
    let ev = run("event: error\ndata: not json\n\n", Flavor::OpenAi);
    assert!(matches!(&ev[0], ProviderEvent::Failed(e) if e.message == "not json"));
}

#[test]
fn vllm_think_blocks_become_reasoning() {
    let frames = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"<thi\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"nk>plan</think>Answer\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
    );
    let ev = run(frames, Flavor::Vllm);
    assert!(ev.contains(&ProviderEvent::ReasoningDelta("plan".into())));
    assert!(ev.contains(&ProviderEvent::TextDelta("Answer".into())));
}

#[test]
fn parses_non_streaming_completion() {
    let (text, usage) = parse_completion(&json!({
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "sum"}]}],
        "usage": {"input_tokens": 4, "output_tokens": 2}
    }));
    assert_eq!(text, "sum");
    assert_eq!(usage.unwrap().output_tokens, 2);
}

#[test]
fn sse_parser_handles_split_chunks_and_comments() {
    let mut p = SseParser::default();
    assert!(p.feed(b": keepalive\n\nevent: a\nda").is_empty());
    let f = p.feed(b"ta: 1\ndata: 2\n\n");
    assert_eq!(f[0].event.as_deref(), Some("a"));
    assert_eq!(f[0].data, "1\n2");
    assert!(p.feed(b"data: tail").is_empty());
    assert_eq!(p.finish().unwrap().data, "tail");
}
