use super::*;
use crate::infra::llm::types::LlmMetadata;
use mini_chat_sdk::{ModelApiParams, WebSearchContextSize};

fn req() -> LlmRequest {
    LlmRequest {
        provider_model_id: "gpt-4.1".into(),
        instructions: "be nice".into(),
        messages: vec![
            LlmMessage::text(LlmRole::User, "hi"),
            LlmMessage::text(LlmRole::Assistant, "hello"),
            LlmMessage {
                role: LlmRole::User,
                parts: vec![
                    LlmPart::Text("what is this".into()),
                    LlmPart::Image { file_id: "file-img".into(), secondary_file_id: None },
                ],
            },
        ],
        max_output_tokens: 100,
        tools: vec![
            LlmTool::FileSearch { vector_store_ids: vec!["vs_1".into()], max_num_results: 5 },
            LlmTool::WebSearch { context_size: WebSearchContextSize::Low },
            LlmTool::CodeInterpreter { file_ids: vec!["file-x".into()] },
        ],
        max_tool_calls: 2,
        api_params: ModelApiParams { temperature: Some(0.7), ..ModelApiParams::default() },
        user: "a".repeat(64),
        metadata: LlmMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat",
            feature: "file_search+web_search+code_interpreter".into(),
        },
        stream: true,
        extra_input: vec![],
    }
}

#[test]
fn body_shape() {
    let b = build_body(&req(), ResponsesFlavor::OPENAI);
    assert_eq!(b["model"], "gpt-4.1");
    assert_eq!(b["instructions"], "be nice");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_output_tokens"], 100);
    assert_eq!(b["max_tool_calls"], 2);
    assert_eq!(b["temperature"], 0.7);
    assert!(b.get("top_p").is_none());
    assert_eq!(b["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(b["input"][1]["role"], "assistant");
    assert_eq!(b["input"][2]["content"][1]["type"], "input_image");
    assert_eq!(b["input"][2]["content"][1]["file_id"], "file-img");
    assert_eq!(b["tools"][0]["type"], "file_search");
    assert_eq!(b["tools"][0]["vector_store_ids"][0], "vs_1");
    assert_eq!(b["tools"][0]["max_num_results"], 5);
    assert_eq!(b["tools"][1]["type"], "web_search");
    assert_eq!(b["tools"][2]["container"]["type"], "auto");
    assert_eq!(b["include"][0], "code_interpreter_call.outputs");
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(b["user"].as_str().unwrap().len(), 64);
}

#[test]
fn vllm_drops_tools_and_metadata() {
    let b = build_body(&req(), ResponsesFlavor::VLLM);
    assert!(b.get("tools").is_none());
    assert!(b.get("metadata").is_none());
    assert!(b.get("max_tool_calls").is_none());
}

#[test]
fn extra_body_cannot_override_controlled_keys() {
    let mut r = req();
    let mut extra = serde_json::Map::new();
    extra.insert("model".into(), json!("evil"));
    extra.insert("seed".into(), json!(7));
    r.api_params.extra_body = Some(extra);
    let b = build_body(&r, ResponsesFlavor::OPENAI);
    assert_eq!(b["model"], "gpt-4.1");
    assert_eq!(b["seed"], 7);
}

#[test]
#[allow(clippy::cognitive_complexity, reason = "sequential stream event assertions")]
fn translates_stream_events() {
    let mut p = ResponsesParser::new(false);
    assert!(p.on_event(Some("response.created"), r#"{"type":"response.created","response":{"id":"resp_1"}}"#).is_empty());
    let ev = p.on_event(Some("response.output_text.delta"), r#"{"delta":"Hel"}"#);
    assert_eq!(ev, vec![ProviderEvent::TextDelta("Hel".into())]);
    // event name taken from data.type when the event line is missing
    let ev = p.on_event(None, r#"{"type":"response.output_text.delta","delta":"lo"}"#);
    assert_eq!(ev, vec![ProviderEvent::TextDelta("lo".into())]);
    let ev = p.on_event(Some("response.file_search_call.searching"), "{}");
    assert!(matches!(&ev[0], ProviderEvent::ToolStart { name, .. } if name == "file_search"));
    let ev = p.on_event(Some("response.file_search_call.completed"), r#"{"results":[1,2]}"#);
    assert!(matches!(&ev[0], ProviderEvent::ToolDone { details, .. } if details["files_searched"] == 2));
    let ev = p.on_event(
        Some("response.output_item.done"),
        r#"{"item":{"type":"code_interpreter_call","outputs":[{"type":"logs","logs":"a"},{"type":"logs","logs":"b"}]}}"#,
    );
    assert!(matches!(&ev[0], ProviderEvent::ToolDone { name, details } if name == "code_interpreter" && details["output"] == "a\nb"));
    let ev = p.on_event(
        Some("response.completed"),
        r#"{"response":{"id":"resp_1","usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":1}},
            "output":[{"type":"message","content":[{"type":"output_text","text":"Hello world","annotations":[
              {"type":"url_citation","url":"https://e.com","title":"E","start_index":6,"end_index":11},
              {"type":"file_citation","file_id":"file-1","filename":"a.pdf","index":0}]}]}]}}"#,
    );
    let ProviderEvent::Completed { usage, response_id, incomplete_reason, citations, output_text } = &ev[0] else {
        panic!("expected completed: {ev:?}");
    };
    assert_eq!(usage.unwrap().cache_read_input_tokens, 3);
    assert_eq!(usage.unwrap().reasoning_tokens, 1);
    assert_eq!(response_id.as_deref(), Some("resp_1"));
    assert!(incomplete_reason.is_none());
    assert_eq!(output_text.as_deref(), Some("Hello world"));
    assert_eq!(
        citations[0],
        RawCitation::Web { url: "https://e.com".into(), title: "E".into(), snippet: "world".into(), span: Some((6, 11)) }
    );
    assert!(matches!(&citations[1], RawCitation::File { file_id, .. } if file_id == "file-1"));
}

#[test]
fn incomplete_and_failures() {
    let mut p = ResponsesParser::new(false);
    let ev = p.on_event(
        Some("response.incomplete"),
        r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}"#,
    );
    assert!(matches!(&ev[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens"));
    let ev = p.on_event(
        Some("response.failed"),
        r#"{"response":{"id":"resp_9","error":{"code":"server_error","message":"boom in file-ABCDEFGHIJKLMNOP"},"usage":{"input_tokens":4,"output_tokens":0}}}"#,
    );
    let ProviderEvent::Failed { kind, message, usage, .. } = &ev[0] else { panic!() };
    assert_eq!(*kind, ProviderFailureKind::ProviderError);
    assert_eq!(message, "boom in [provider_id]");
    assert_eq!(usage.unwrap().input_tokens, 4);
    let ev = p.on_event(Some("error"), r#"{"code":"x","message":"flat error"}"#);
    assert!(matches!(&ev[0], ProviderEvent::Failed { message, .. } if message == "flat error"));
    let ev = p.on_event(Some("error"), "not json at all");
    assert!(matches!(&ev[0], ProviderEvent::Failed { message, .. } if message == "not json at all"));
}

#[test]
fn snippet_out_of_range_is_empty() {
    let a = json!({"type":"url_citation","url":"u","title":"t","start_index":5,"end_index":50});
    assert!(matches!(annotation_to_citation(&a, "short"), Some(RawCitation::Web { snippet, .. }) if snippet.is_empty()));
    let b = json!({"type":"url_citation","url":"u","title":"t"});
    assert!(matches!(annotation_to_citation(&b, "x"), Some(RawCitation::Web { span: None, .. })));
}

#[test]
fn think_blocks_become_reasoning() {
    let mut p = ResponsesParser::new(true);
    let ev = p.on_event(Some("response.output_text.delta"), r#"{"delta":"<think>plan</think>answer"}"#);
    assert_eq!(ev, vec![ProviderEvent::ReasoningDelta("plan".into()), ProviderEvent::TextDelta("answer".into())]);
}

#[test]
fn non_streaming_parse() {
    let v = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"S"}]}],"usage":{"input_tokens":5,"output_tokens":2}});
    let (t, u) = parse_non_streaming(&v);
    assert_eq!(t, "S");
    assert_eq!(u.unwrap().output_tokens, 2);
}
