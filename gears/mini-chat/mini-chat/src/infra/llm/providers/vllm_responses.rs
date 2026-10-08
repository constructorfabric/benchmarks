//! vLLM Responses adapter (DESIGN section 3.2 `llm_provider`, section 3.3
//! `delta` event, section 4 "Provider Request Metadata").
//!
//! The Responses wire format of [`super::openai_responses`] with these
//! differences:
//! - every tool is dropped, function tools included (so no `include` and no
//!   `max_tool_calls`), and `metadata` is not sent; `user` is;
//! - text inside `<think>…</think>` is emitted as [`LlmEvent::ReasoningDelta`]
//!   (tags split across deltas are handled), as are
//!   `response.reasoning_text.delta` events; the non-streaming reply has its
//!   think blocks removed.

use serde_json::Value;

use super::openai_responses;
use super::stream::Translate;
use crate::infra::llm::sse::SseFrame;
use crate::infra::llm::types::{CompletionResult, LlmEvent, LlmRequest, ProviderError};

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// The Responses body without tools and metadata.
pub(super) fn build_body(req: &LlmRequest) -> Value {
    let mut body = openai_responses::build_body(req);
    if let Some(obj) = body.as_object_mut() {
        for key in ["tools", "include", "max_tool_calls", "metadata"] {
            obj.remove(key);
        }
    }
    body
}

/// The Responses reply with think blocks removed from the text.
pub(super) fn parse_completion(bytes: &[u8]) -> Result<CompletionResult, ProviderError> {
    let mut out = openai_responses::parse_completion(bytes)?;
    let mut splitter = ThinkSplitter::default();
    let mut text = String::new();
    for ev in splitter.push(&out.text).into_iter().chain(splitter.flush()) {
        if let LlmEvent::TextDelta(t) = ev {
            text.push_str(&t);
        }
    }
    out.text = text;
    Ok(out)
}

/// Responses translation with `<think>` splitting of the text deltas.
pub(super) struct Translator {
    inner: openai_responses::Translator,
    think: ThinkSplitter,
}

impl Default for Translator {
    fn default() -> Self {
        Self {
            inner: openai_responses::Translator::with_reasoning_text(),
            think: ThinkSplitter::default(),
        }
    }
}

impl Translate for Translator {
    fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        for ev in self.inner.on_frame(frame) {
            match ev {
                LlmEvent::TextDelta(text) => out.extend(self.think.push(&text)),
                LlmEvent::Completed { .. } | LlmEvent::Failed { .. } => {
                    out.extend(self.think.flush());
                    out.push(ev);
                }
                other => out.push(other),
            }
        }
        out
    }
}

/// Splits streamed text into text and reasoning at `<think>` / `</think>`. A
/// suffix that may be the start of the next tag is held back until the next
/// chunk (or [`Self::flush`]).
#[derive(Default)]
struct ThinkSplitter {
    in_think: bool,
    pending: String,
}

impl ThinkSplitter {
    fn push(&mut self, chunk: &str) -> Vec<LlmEvent> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(chunk);
        let mut out = Vec::new();
        let mut rest = buf.as_str();
        loop {
            let tag = if self.in_think {
                THINK_CLOSE
            } else {
                THINK_OPEN
            };
            if let Some(pos) = rest.find(tag) {
                self.emit(&rest[..pos], &mut out);
                self.in_think = !self.in_think;
                rest = &rest[pos + tag.len()..];
                continue;
            }
            let keep = partial_tag_len(rest, tag);
            let (now, later) = rest.split_at(rest.len() - keep);
            self.emit(now, &mut out);
            later.clone_into(&mut self.pending);
            return out;
        }
    }

    /// The held-back text, in the current mode.
    fn flush(&mut self) -> Vec<LlmEvent> {
        let pending = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        self.emit(&pending, &mut out);
        out
    }

    fn emit(&self, text: &str, out: &mut Vec<LlmEvent>) {
        if text.is_empty() {
            return;
        }
        out.push(if self.in_think {
            LlmEvent::ReasoningDelta(text.to_owned())
        } else {
            LlmEvent::TextDelta(text.to_owned())
        });
    }
}

/// Length of the longest suffix of `text` that is a proper prefix of `tag`.
fn partial_tag_len(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| text.ends_with(&tag[..n]))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "vllm_responses_tests.rs"]
mod vllm_responses_tests;
