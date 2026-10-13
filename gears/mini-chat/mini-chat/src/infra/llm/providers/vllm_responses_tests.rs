use serde_json::json;

use super::*;
use crate::infra::llm::providers::test_util::{TENANT, USER, all_tools, ev, parse_all, req_with};
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::LlmTerminal;

fn delta(text: &str) -> SseEvent {
    ev(
        Some("response.output_text.delta"),
        &json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": text}),
    )
}

fn completed() -> SseEvent {
    ev(
        Some("response.completed"),
        &json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 3, "output_tokens": 4}}}),
    )
}

#[test]
fn drops_all_tools_no_metadata() {
    let b = VllmResponsesAdapter.build_body(&req_with(all_tools()));
    assert_eq!(b["model"], "model-x");
    assert_eq!(b["instructions"], "SYSTEM PROMPT");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_output_tokens"], 4096);
    assert_eq!(b["user"], provider_user_field(TENANT, USER));
    assert_eq!(b["input"].as_array().map(Vec::len), Some(3));
    for absent in ["tools", "include", "max_tool_calls", "metadata"] {
        assert!(b.get(absent).is_none(), "{absent} must not be sent: {b}");
    }
}

#[test]
fn think_block_becomes_reasoning_delta() {
    let events = parse_all(
        &VllmResponsesAdapter,
        &[
            delta("<think>plan"),
            delta(" more</th"),
            delta("ink>Answer"),
            delta(" <"),
            delta("b>"),
            completed(),
        ],
    );
    let usage = mini_chat_sdk::UsageTokens {
        input_tokens: 3,
        output_tokens: 4,
        ..Default::default()
    };
    assert_eq!(
        events,
        vec![
            LlmEvent::ReasoningDelta("plan".into()),
            LlmEvent::ReasoningDelta(" more".into()),
            LlmEvent::TextDelta("Answer".into()),
            LlmEvent::TextDelta(" ".into()),
            LlmEvent::TextDelta("<b>".into()),
            LlmEvent::Completed(LlmTerminal {
                usage: Some(usage),
                response_id: Some("resp_1".into()),
            }),
        ]
    );
}

#[test]
fn partial_tag_at_end_is_flushed_as_text_before_terminal() {
    let events = parse_all(&VllmResponsesAdapter, &[delta("a <thi"), completed()]);
    assert_eq!(events[0], LlmEvent::TextDelta("a ".into()));
    assert_eq!(events[1], LlmEvent::TextDelta("<thi".into()));
    assert!(events[2].is_terminal());
}
