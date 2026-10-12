//! The vLLM Responses API adapter (`vllm_responses`): the Responses wire format without built-in
//! tools and without `metadata`; text inside `<think>` blocks is streamed as reasoning.

use serde_json::Value;

use super::adapter::{Adapter, EventParser};
use super::openai_responses::{ResponsesParser, build_responses_body, parse_responses_completion};
use crate::domain::llm::{LlmCompletion, LlmEvent, LlmRequest, ProviderError};

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

/// The vLLM Responses adapter.
pub struct VllmResponses;

impl Adapter for VllmResponses {
    fn build_body(&self, request: &LlmRequest, stream: bool) -> Result<Value, ProviderError> {
        Ok(build_responses_body(request, stream, false, false))
    }

    fn parser(&self) -> Box<dyn EventParser> {
        Box::new(VllmParser {
            inner: ResponsesParser::default(),
            splitter: ThinkSplitter::default(),
        })
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderError> {
        let mut completion = parse_responses_completion(body)?;
        let mut splitter = ThinkSplitter::default();
        let mut visible = String::new();
        for event in splitter
            .feed(&completion.text)
            .into_iter()
            .chain(splitter.flush())
        {
            if let LlmEvent::TextDelta(text) = event {
                visible.push_str(&text);
            }
        }
        completion.text = visible;
        Ok(completion)
    }
}

struct VllmParser {
    inner: ResponsesParser,
    splitter: ThinkSplitter,
}

impl EventParser for VllmParser {
    fn push(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        for translated in self.inner.push(event, data) {
            match translated {
                LlmEvent::TextDelta(text) => out.extend(self.splitter.feed(&text)),
                terminal @ (LlmEvent::Completed { .. } | LlmEvent::Failed { .. }) => {
                    out.extend(self.splitter.flush());
                    out.push(terminal);
                }
                other => out.push(other),
            }
        }
        if event == Some("response.reasoning_text.delta")
            && let Ok(json) = serde_json::from_str::<Value>(data)
            && let Some(delta) = json.get("delta").and_then(Value::as_str)
        {
            out.push(LlmEvent::ReasoningDelta(delta.to_owned()));
        }
        out
    }
}

/// Splits a text stream into reasoning (inside `<think>...</think>`) and answer text. Fragments
/// that may be the start of a tag are held back until the next chunk decides.
#[derive(Default)]
struct ThinkSplitter {
    in_think: bool,
    pending: String,
}

impl ThinkSplitter {
    fn emit(&self, text: String) -> Option<LlmEvent> {
        if text.is_empty() {
            None
        } else if self.in_think {
            Some(LlmEvent::ReasoningDelta(text))
        } else {
            Some(LlmEvent::TextDelta(text))
        }
    }

    fn feed(&mut self, chunk: &str) -> Vec<LlmEvent> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { CLOSE_TAG } else { OPEN_TAG };
            if let Some(position) = self.pending.find(tag) {
                let before: String = self.pending.drain(..position).collect();
                out.extend(self.emit(before));
                self.pending.drain(..tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // Hold back the longest suffix that is a prefix of the tag.
            let hold = (1..tag.len())
                .rev()
                .find(|&len| self.pending.ends_with(&tag[..len]))
                .unwrap_or(0);
            let ready_len = self.pending.len() - hold;
            let ready: String = self.pending.drain(..ready_len).collect();
            out.extend(self.emit(ready));
            return out;
        }
    }

    fn flush(&mut self) -> Vec<LlmEvent> {
        let rest = std::mem::take(&mut self.pending);
        self.emit(rest).into_iter().collect()
    }
}
