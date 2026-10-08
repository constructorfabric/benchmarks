//! vLLM Responses API adapter (`kind: vllm_responses`, S§9.2, ADR-0005).
//!
//! The Responses wire format of [`OpenAiResponsesAdapter`] without the
//! `metadata` object, `max_tool_calls` and every tool (function tools
//! included). Answer text inside `<think>…</think>` is reasoning: it is
//! emitted as [`LlmEvent::ReasoningDelta`] (SSE `delta` type `reasoning`).

use serde_json::Value;

use super::{OpenAiResponsesAdapter, ParseState, ProviderAdapter};
use crate::infra::llm::sse_parser::SseEvent;
use crate::infra::llm::types::{LlmCompletion, LlmEvent, LlmRequest, ProviderFailure};

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Body keys of the Responses format that vLLM requests do not carry.
const DROPPED_KEYS: &[&str] = &["metadata", "tools", "include", "max_tool_calls"];

/// vLLM Responses adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct VllmResponsesAdapter;

impl ProviderAdapter for VllmResponsesAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = OpenAiResponsesAdapter.build_body(req);
        if let Some(obj) = body.as_object_mut() {
            for key in DROPPED_KEYS {
                obj.remove(*key);
            }
        }
        body
    }

    fn parse_event(&self, event: &SseEvent, state: &mut ParseState) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        for ev in OpenAiResponsesAdapter.parse_event(event, state) {
            match ev {
                LlmEvent::TextDelta(text) => split_think(&text, state, &mut out),
                terminal if terminal.is_terminal() => {
                    flush_pending(state, &mut out);
                    out.push(terminal);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderFailure> {
        OpenAiResponsesAdapter.parse_completion(body)
    }
}

/// Split a text delta at `<think>` / `</think>` tags (tags may span deltas:
/// a trailing partial tag is held back until the next delta).
fn split_think(delta: &str, state: &mut ParseState, out: &mut Vec<LlmEvent>) {
    let mut buf = std::mem::take(&mut state.think_pending);
    buf.push_str(delta);
    loop {
        let tag = if state.in_think {
            THINK_CLOSE
        } else {
            THINK_OPEN
        };
        if let Some(idx) = buf.find(tag) {
            emit(&buf[..idx], state.in_think, out);
            buf.drain(..idx + tag.len());
            state.in_think = !state.in_think;
            continue;
        }
        let keep = partial_tag_len(&buf, tag);
        let split = buf.len() - keep;
        emit(&buf[..split], state.in_think, out);
        buf[split..].clone_into(&mut state.think_pending);
        return;
    }
}

/// Length of the longest suffix of `s` that is a proper prefix of `tag`.
fn partial_tag_len(s: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| s.ends_with(&tag[..n]))
        .unwrap_or(0)
}

/// Text held back at the end of the stream is emitted as is.
fn flush_pending(state: &mut ParseState, out: &mut Vec<LlmEvent>) {
    let pending = std::mem::take(&mut state.think_pending);
    emit(&pending, state.in_think, out);
}

fn emit(text: &str, reasoning: bool, out: &mut Vec<LlmEvent>) {
    if text.is_empty() {
        return;
    }
    out.push(if reasoning {
        LlmEvent::ReasoningDelta(text.to_owned())
    } else {
        LlmEvent::TextDelta(text.to_owned())
    });
}

#[cfg(test)]
#[path = "vllm_responses_tests.rs"]
mod tests;
