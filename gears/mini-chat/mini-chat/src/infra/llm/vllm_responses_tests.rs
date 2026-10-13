#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};

use super::VllmResponsesAdapter;
use crate::infra::llm::adapter_fixtures::{TENANT, USER, feed, request};
use crate::infra::llm::{LlmEvent, LlmTool, ProviderAdapter};

fn delta(text: &str) -> (&'static str, Value) {
    (
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": text}),
    )
}

fn completed() -> (&'static str, Value) {
    (
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": "resp_v1", "status": "completed", "output": [],
            "usage": {"input_tokens": 4, "output_tokens": 6},
        }}),
    )
}

#[test]
fn drops_all_tools_and_metadata() {
    let req = request(vec![
        LlmTool::FileSearch {
            vector_store_id: "vs_1".to_owned(),
            max_num_results: 5,
        },
        LlmTool::WebSearch {
            context_size: "low".to_owned(),
        },
        LlmTool::CodeInterpreter {
            file_ids: vec!["file-x".to_owned()],
        },
        LlmTool::SearchKnowledge,
    ]);
    let b = VllmResponsesAdapter.build_body(&req);

    assert!(b.get("tools").is_none());
    assert!(b.get("max_tool_calls").is_none());
    assert!(b.get("include").is_none());
    assert!(b.get("metadata").is_none());
    // Otherwise the Responses format.
    assert_eq!(b["model"], json!("model-x"));
    assert_eq!(b["instructions"], json!("Be helpful."));
    assert_eq!(b["max_output_tokens"], json!(4096));
    assert_eq!(b["stream"], json!(true));
    assert_eq!(
        b["user"],
        json!(format!("{}{}", TENANT.simple(), USER.simple()))
    );
    assert_eq!(b["input"].as_array().unwrap().len(), 3);
    assert_eq!(
        b["input"][0],
        json!({"role": "user", "content": "earlier question"})
    );
}

#[test]
fn think_blocks_become_reasoning_deltas() {
    // Tags split across deltas.
    let events = feed(
        &VllmResponsesAdapter,
        &[
            delta("<thi"),
            delta("nk>plan"),
            delta(" more</th"),
            delta("ink>Answer"),
            completed(),
        ],
    );
    assert_eq!(
        events[..3],
        [
            LlmEvent::ReasoningDelta("plan".to_owned()),
            LlmEvent::ReasoningDelta(" more".to_owned()),
            LlmEvent::TextDelta("Answer".to_owned()),
        ]
    );
    assert!(
        matches!(&events[3], LlmEvent::Completed { response_id: Some(id), incomplete_reason: None, .. } if id == "resp_v1"),
        "{events:?}"
    );
    assert_eq!(events.len(), 4);

    // A whole block inside one delta, text around it.
    let events = feed(
        &VllmResponsesAdapter,
        &[delta("Hi <think>hmm</think> there"), completed()],
    );
    assert_eq!(
        events[..3],
        [
            LlmEvent::TextDelta("Hi ".to_owned()),
            LlmEvent::ReasoningDelta("hmm".to_owned()),
            LlmEvent::TextDelta(" there".to_owned()),
        ]
    );

    // A held partial tag that never completes is text, flushed before the terminal event.
    let events = feed(&VllmResponsesAdapter, &[delta("x <"), completed()]);
    assert_eq!(
        events[..2],
        [
            LlmEvent::TextDelta("x ".to_owned()),
            LlmEvent::TextDelta("<".to_owned()),
        ]
    );
    assert!(events[2].is_terminal());
}

#[test]
fn summary_completion_drops_think_blocks() {
    let body = json!({
        "id": "resp_s", "status": "completed",
        "output": [{"type": "message", "content": [
            {"type": "output_text", "text": "<think>draft</think>Final summary"},
        ]}],
        "usage": {"input_tokens": 8, "output_tokens": 3},
    });
    let r = VllmResponsesAdapter
        .parse_completion(body.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.text, "Final summary");
    assert_eq!(r.usage.unwrap().output_tokens, 3);
}
