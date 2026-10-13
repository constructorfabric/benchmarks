use mini_chat_sdk::ModelApiParams;
use serde_json::json;

use super::*;

fn req(tools: Vec<ToolSpec>) -> LlmRequest {
    let mut metadata = serde_json::Map::new();
    metadata.insert("request_type".into(), json!("chat"));
    LlmRequest {
        provider_model_id: "gpt-4.1".into(),
        instructions: "be nice".into(),
        input: vec![
            InputMessage::text("user", "hi"),
            InputMessage::text("assistant", "hello"),
            InputMessage { role: "user".into(), parts: vec![ContentPart::Text("see".into()), ContentPart::Image("file-abc".into())] },
        ],
        max_output_tokens: 1024,
        tools,
        max_tool_calls: 2,
        user: provider_user(uuid::Uuid::nil(), uuid::Uuid::nil()),
        metadata,
        api_params: ModelApiParams { temperature: Some(0.7), ..Default::default() },
        stream: true,
    }
}

#[test]
fn responses_body_shape() {
    let tools = vec![
        ToolSpec::FileSearch { vector_store_ids: vec!["vs_1".into()], max_num_results: 5 },
        ToolSpec::WebSearch { search_context_size: "low".into() },
        ToolSpec::CodeInterpreter { file_ids: vec!["file-x".into()] },
    ];
    let b = build_body(ProviderKind::OpenaiResponses, &req(tools.clone()));
    assert_eq!(b["model"], "gpt-4.1");
    assert_eq!(b["stream"], true);
    assert_eq!(b["instructions"], "be nice");
    assert_eq!(b["max_output_tokens"], 1024);
    assert_eq!(b["user"].as_str().unwrap().len(), 64);
    assert_eq!(b["input"][0]["content"], "hi");
    assert_eq!(b["input"][2]["content"][1]["type"], "input_image");
    assert_eq!(b["input"][2]["content"][1]["file_id"], "file-abc");
    assert_eq!(b["tools"][0]["type"], "file_search");
    assert_eq!(b["tools"][0]["vector_store_ids"][0], "vs_1");
    assert_eq!(b["tools"][2]["container"]["file_ids"][0], "file-x");
    assert_eq!(b["include"][0], "code_interpreter_call.outputs");
    assert_eq!(b["max_tool_calls"], 2);
    assert_eq!(b["temperature"], 0.7);
    assert_eq!(feature_label(&tools), "file_search+web_search+code_interpreter");
    let v = build_body(ProviderKind::VllmResponses, &req(tools));
    assert!(v.get("tools").is_none());
    assert!(v.get("metadata").is_none());
}

#[test]
fn extra_body_reserved_keys_ignored() {
    let mut r = req(vec![]);
    let mut extra = serde_json::Map::new();
    extra.insert("model".into(), json!("evil"));
    extra.insert("custom".into(), json!(1));
    r.api_params.extra_body = Some(extra);
    let b = build_body(ProviderKind::OpenaiResponses, &r);
    assert_eq!(b["model"], "gpt-4.1");
    assert_eq!(b["custom"], 1);
}

fn tr(event: Option<&str>, data: serde_json::Value) -> Vec<ProviderEvent> {
    let mut st = TranslateState::default();
    translate(ProviderKind::OpenaiResponses, &mut st, event, &data.to_string())
}

#[test]
fn translates_text_tools_and_completion() {
    assert_eq!(
        tr(None, json!({"type":"response.output_text.delta","delta":"Hi"})),
        vec![ProviderEvent::TextDelta("Hi".into())]
    );
    assert_eq!(
        tr(Some("response.web_search_call.searching"), json!({})),
        vec![ProviderEvent::ToolStart { name: "web_search".into(), details: json!({}) }]
    );
    assert_eq!(
        tr(Some("response.file_search_call.completed"), json!({})),
        vec![ProviderEvent::ToolDone { name: "file_search".into(), details: json!({"files_searched": 0}) }]
    );
    let done = tr(None, json!({"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}}}}));
    match &done[0] {
        ProviderEvent::Completed { usage: Some(u), response_id, incomplete_reason } => {
            assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_input_tokens, u.reasoning_tokens), (10, 5, 2, 1));
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert!(incomplete_reason.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
    let inc = tr(None, json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}));
    assert!(matches!(&inc[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens"));
}

#[test]
fn translates_failures_and_annotations() {
    let f = tr(Some("response.failed"), json!({"response":{"error":{"message":"boom"},"usage":{"input_tokens":3,"output_tokens":0}}}));
    assert!(matches!(&f[0], ProviderEvent::Failed { message, usage: Some(u), .. } if message == "boom" && u.input_tokens == 3));
    let e = tr(Some("error"), json!({"code":"x","message":"flat"}));
    assert!(matches!(&e[0], ProviderEvent::Failed { message, .. } if message == "flat"));
    let mut st = TranslateState::default();
    translate(ProviderKind::OpenaiResponses, &mut st, None, &json!({"type":"response.output_text.delta","delta":"Hello world"}).to_string());
    let a = translate(ProviderKind::OpenaiResponses, &mut st, None, &json!({"type":"response.output_text.annotation.added","annotation":{"type":"url_citation","url":"https://e.x","title":"T","start_index":0,"end_index":5}}).to_string());
    assert_eq!(a, vec![ProviderEvent::Citation(RawCitation::Web { url: "https://e.x".into(), title: "T".into(), snippet: "Hello".into(), span: Some((0, 5)) })]);
    let c = tr(None, json!({"type":"response.output_item.done","item":{"type":"code_interpreter_call","outputs":[{"type":"logs","logs":"a"},{"type":"logs","logs":"b"}]}}));
    assert_eq!(c, vec![ProviderEvent::ToolDone { name: "code_interpreter".into(), details: json!({"output":"a\nb"}) }]);
}

#[test]
fn vllm_think_blocks_become_reasoning() {
    let mut st = TranslateState::default();
    let ev = translate(ProviderKind::VllmResponses, &mut st, None, &json!({"type":"response.output_text.delta","delta":"<think>plan</think>answer"}).to_string());
    assert_eq!(ev, vec![ProviderEvent::ReasoningDelta("plan".into()), ProviderEvent::TextDelta("answer".into())]);
}

#[test]
fn chat_completions_and_anthropic_streams() {
    let mut st = TranslateState::default();
    let a = translate(ProviderKind::OpenaiChatCompletions, &mut st, None, &json!({"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}).to_string());
    assert_eq!(a, vec![ProviderEvent::TextDelta("x".into())]);
    let b = translate(ProviderKind::OpenaiChatCompletions, &mut st, None, &json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2}}).to_string());
    assert!(matches!(&b[0], ProviderEvent::Completed { usage: Some(u), incomplete_reason: Some(r), .. } if u.input_tokens == 4 && r == "max_tokens"));
    let mut st = TranslateState::default();
    translate(ProviderKind::AnthropicMessages, &mut st, Some("message_start"), &json!({"message":{"usage":{"input_tokens":7}}}).to_string());
    let t = translate(ProviderKind::AnthropicMessages, &mut st, Some("content_block_delta"), &json!({"delta":{"type":"text_delta","text":"y"}}).to_string());
    assert_eq!(t, vec![ProviderEvent::TextDelta("y".into())]);
    translate(ProviderKind::AnthropicMessages, &mut st, Some("message_delta"), &json!({"usage":{"output_tokens":3}}).to_string());
    let d = translate(ProviderKind::AnthropicMessages, &mut st, Some("message_stop"), "{}");
    assert!(matches!(&d[0], ProviderEvent::Completed { usage: Some(u), .. } if u.input_tokens == 7 && u.output_tokens == 3));
}

#[test]
fn error_body_message_extraction() {
    assert_eq!(error_message_from_body(br#"{"error":{"message":"bad"}}"#), "bad");
    assert_eq!(error_message_from_body(b"plain"), "plain");
}
