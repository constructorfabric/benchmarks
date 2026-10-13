//! The `vllm_responses` adapter: the Responses request body without tools
//! (`cpt-cf-mini-chat-dod-streaming-turns-other-adapters`) and the Responses
//! translation with `<think>` text relayed as reasoning.

use serde_json::Value;

use super::openai_responses::{OpenAiResponsesAdapter, ResponsesTranslator, continue_responses};
use super::{EventTranslator, ProviderAdapter};
use crate::config::ProviderKind;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{BuiltRequest, ToolCallOutput};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::tools::ToolsDecision;
use crate::domain::service::stream::types::ProviderEvent;
use crate::infra::llm::sse_parse::SseEvent;

/// The vLLM Responses adapter.
#[derive(Debug, Default)]
pub struct VllmResponsesAdapter;

impl VllmResponsesAdapter {
    /// Creates the adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ProviderAdapter for VllmResponsesAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::VllmResponses
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-vllm
    /// The Responses body of the turn without `tools`, `include`,
    /// `max_tool_calls` and `metadata`: every tool, function tools included,
    /// is dropped.
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        let no_tools = ToolsDecision::default();
        let stripped = ProviderRequestInput {
            tools: &no_tools,
            ..*input
        };
        let mut built = OpenAiResponsesAdapter::new().build_request(&stripped)?;
        if let Value::Object(body) = &mut built.body {
            body.remove("metadata");
        }
        built.kind = ProviderKind::VllmResponses;
        Ok(built)
    }

    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-build-provider-request:p1:inst-case-vllm
    fn continue_request(
        &self,
        previous: &BuiltRequest,
        outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError> {
        continue_responses(previous, outputs)
    }

    fn translator(&self) -> Box<dyn EventTranslator> {
        Box::new(VllmTranslator::default())
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-vllm-think
const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Splits the text deltas at the `<think>` and `</think>` tags, which may be
/// split across deltas.
#[derive(Debug, Default)]
struct ThinkSplitter {
    in_think: bool,
    pending: String,
    item_id: Option<String>,
    content_index: Option<u32>,
}

impl ThinkSplitter {
    fn emit(&self, segment: &str, out: &mut Vec<ProviderEvent>) {
        if segment.is_empty() {
            return;
        }
        out.push(if self.in_think {
            ProviderEvent::ReasoningDelta {
                text: segment.to_owned(),
            }
        } else {
            ProviderEvent::TextDelta {
                text: segment.to_owned(),
                item_id: self.item_id.clone(),
                content_index: self.content_index,
            }
        });
    }

    fn push(
        &mut self,
        text: &str,
        item_id: Option<String>,
        content_index: Option<u32>,
    ) -> Vec<ProviderEvent> {
        self.item_id = item_id;
        self.content_index = content_index;
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.push_str(text);
        let mut out = Vec::new();
        let mut rest = buffer.as_str();
        loop {
            let tag = if self.in_think { CLOSE } else { OPEN };
            if let Some(position) = rest.find(tag) {
                self.emit(&rest[..position], &mut out);
                rest = &rest[position + tag.len()..];
                self.in_think = !self.in_think;
                continue;
            }
            // A suffix that could be the start of the tag waits for the next delta.
            let keep = (1..tag.len())
                .rev()
                .find(|len| rest.ends_with(&tag[..*len]))
                .unwrap_or(0);
            let (now, later) = rest.split_at(rest.len() - keep);
            self.emit(now, &mut out);
            later.clone_into(&mut self.pending);
            return out;
        }
    }

    fn flush(&mut self) -> Vec<ProviderEvent> {
        let pending = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        self.emit(&pending, &mut out);
        out
    }
}

/// The Responses translation plus the `<think>` handling.
#[derive(Debug, Default)]
struct VllmTranslator {
    inner: ResponsesTranslator,
    think: ThinkSplitter,
}

impl EventTranslator for VllmTranslator {
    fn translate(&mut self, event: &SseEvent) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        for translated in self.inner.translate(event) {
            match translated {
                ProviderEvent::TextDelta {
                    text,
                    item_id,
                    content_index,
                } => out.extend(self.think.push(&text, item_id, content_index)),
                terminal @ (ProviderEvent::Completed { .. }
                | ProviderEvent::ToolUse { .. }
                | ProviderEvent::ProviderError { .. }) => {
                    out.extend(self.think.flush());
                    out.push(terminal);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        let mut out = self.think.flush();
        out.extend(self.inner.finish());
        out
    }
}

// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream:p1:inst-vllm-think
#[cfg(test)]
#[path = "vllm_responses_tests.rs"]
mod tests;
