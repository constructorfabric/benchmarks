//! vLLM Responses adapter (`vllm_responses`): Responses wire format without
//! tools and metadata; text inside `<think>` blocks becomes reasoning deltas.

use serde_json::Value;

use super::openai_responses::{ResponsesDecoder, build_body as responses_body};
use super::sse::SseFrame;
use super::{ChatRequest, ProviderEvent};

#[must_use]
pub fn build_body(req: &ChatRequest) -> Value {
    responses_body(req, false, false, false)
}

#[derive(Debug, Default)]
pub struct VllmDecoder {
    inner: ResponsesDecoder,
    in_think: bool,
    carry: String,
}

impl VllmDecoder {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.inner.is_terminal()
    }

    fn split(&mut self, text: &str, out: &mut Vec<ProviderEvent>) {
        let mut s = std::mem::take(&mut self.carry);
        s.push_str(text);
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = s.find(tag) {
                let (before, after) = s.split_at(pos);
                if !before.is_empty() {
                    out.push(if self.in_think {
                        ProviderEvent::ReasoningDelta(before.to_owned())
                    } else {
                        ProviderEvent::TextDelta(before.to_owned())
                    });
                }
                self.in_think = !self.in_think;
                s = after[tag.len()..].to_owned();
            } else {
                // Keep a possible partial tag for the next delta.
                let keep = (1..tag.len()).rev().find(|&k| s.len() >= k && tag.starts_with(&s[s.len() - k..])).unwrap_or(0);
                let emit_len = s.len() - keep;
                if emit_len > 0 && s.is_char_boundary(emit_len) {
                    let emit = s[..emit_len].to_owned();
                    out.push(if self.in_think {
                        ProviderEvent::ReasoningDelta(emit)
                    } else {
                        ProviderEvent::TextDelta(emit)
                    });
                    self.carry = s[emit_len..].to_owned();
                } else {
                    self.carry = s;
                }
                break;
            }
        }
    }

    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        for ev in self.inner.on_frame(frame) {
            match ev {
                ProviderEvent::TextDelta(t) => self.split(&t, &mut out),
                ProviderEvent::Completed { .. } | ProviderEvent::Failed { .. } => {
                    if !self.carry.is_empty() {
                        let c = std::mem::take(&mut self.carry);
                        out.push(if self.in_think { ProviderEvent::ReasoningDelta(c) } else { ProviderEvent::TextDelta(c) });
                    }
                    out.push(ev);
                }
                ProviderEvent::ToolStart { .. } | ProviderEvent::ToolDone { .. } => {}
                other => out.push(other),
            }
        }
        out
    }
}
