//! `vllm_responses` adapter: the vLLM Responses API (spec §11.5; DESIGN §3.2
//! `llm_provider`, §3.3 `event: delta`, §4 "Provider Request Metadata").
//!
//! The request is the Responses body without tools (function tools included),
//! `max_tool_calls`, `include` and `metadata`. Events are translated like the
//! Responses API, except that text inside `<think>…</think>` becomes
//! `reasoning` deltas; a tag may be split across deltas, so text that could
//! start a tag is held until the next delta (or the terminal event).

use serde_json::Value;

use super::openai_responses::{self, OpenAiResponsesAdapter};
use super::{CompletionResult, LlmError, LlmEvent, LlmRequest, ParseState, ProviderAdapter};

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

/// Responses keys vLLM does not get.
const DROPPED_KEYS: &[&str] = &["tools", "max_tool_calls", "include", "metadata"];

/// vLLM Responses API adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct VllmResponsesAdapter;

/// Byte length of the longest suffix of `text` that is a proper prefix of `tag`.
fn partial_tag_len(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| text.ends_with(&tag[..n]))
        .unwrap_or(0)
}

fn piece(in_think: bool, text: &str, out: &mut Vec<LlmEvent>) {
    if text.is_empty() {
        return;
    }
    out.push(if in_think {
        LlmEvent::ReasoningDelta(text.to_owned())
    } else {
        LlmEvent::TextDelta(text.to_owned())
    });
}

/// Split held text plus `delta` into text / reasoning pieces.
fn split_think(st: &mut ParseState, delta: &str, out: &mut Vec<LlmEvent>) {
    let mut buf = std::mem::take(&mut st.held);
    buf.push_str(delta);
    let mut rest = buf.as_str();
    loop {
        let tag = if st.in_think { CLOSE_TAG } else { OPEN_TAG };
        if let Some(pos) = rest.find(tag) {
            piece(st.in_think, &rest[..pos], out);
            rest = &rest[pos + tag.len()..];
            st.in_think = !st.in_think;
            continue;
        }
        let keep = partial_tag_len(rest, tag);
        piece(st.in_think, &rest[..rest.len() - keep], out);
        rest[rest.len() - keep..].clone_into(&mut st.held);
        return;
    }
}

impl ProviderAdapter for VllmResponsesAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = openai_responses::build(req);
        if let Some(obj) = body.as_object_mut() {
            for key in DROPPED_KEYS {
                obj.remove(*key);
            }
        }
        body
    }

    fn parse_event(&self, st: &mut ParseState, event: &str, data: &str) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        for ev in OpenAiResponsesAdapter.parse_event(st, event, data) {
            match ev {
                LlmEvent::TextDelta(delta) => split_think(st, &delta, &mut out),
                terminal if terminal.is_terminal() => {
                    let held = std::mem::take(&mut st.held);
                    piece(st.in_think, &held, &mut out);
                    out.push(terminal);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn parse_completion(&self, body: &[u8]) -> Result<CompletionResult, LlmError> {
        let mut r = OpenAiResponsesAdapter.parse_completion(body)?;
        let mut st = ParseState::default();
        let mut events = Vec::new();
        split_think(&mut st, &r.text, &mut events);
        piece(st.in_think, &st.held, &mut events);
        r.text = events
            .into_iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta(t) => Some(t),
                _ => None,
            })
            .collect();
        Ok(r)
    }
}

#[cfg(test)]
#[path = "vllm_responses_tests.rs"]
mod vllm_responses_tests;
