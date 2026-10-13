//! `vllm_responses` adapter: vLLM Responses API (DESIGN section 3.2
//! `llm_provider`, section 3.3 `event: delta`, "Provider Event Translation",
//! "Provider Request Metadata", "Model Catalog Configuration", ADR-0005).
//!
//! The request is the Responses protocol of [`super::openai_responses`]
//! reduced to what vLLM serves: `model`, `instructions`, `input`, `stream`,
//! `max_output_tokens`, `user`, the catalog sampling parameters and the
//! merged `extra_body` (keys the request controls are ignored). It carries no
//! tools (every tool, function tools included, is dropped), no `metadata`,
//! no `include` and no `max_tool_calls`.
//!
//! The stream is translated by the Responses translator with `<think>`
//! splitting ([`ThinkSplitter`]): text inside `<think>` … `</think>` is
//! streamed as reasoning deltas, the rest as text (the tags may be split
//! across deltas); `response.reasoning_text.delta` is reasoning too. The
//! answer (text deltas, completed `output_text`, the non-streaming summary
//! text) never contains the reasoning. Errors are mapped like the `OpenAI`
//! Responses adapter.

use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::openai_responses::{
    ResponsesTranslator, input_item, insert_api_params, merge_extra_body, parse_completion,
};
use super::sse::event_stream;
use super::transport::{CANCELLED_MESSAGE, COMPLETION_BODY_LIMIT, ProviderTransport, read_limited};
use crate::domain::llm::{
    LlmCompletion, LlmEventStream, LlmFailure, LlmProvider, LlmRequest, ProviderTarget,
};
use crate::infra::oagw_provisioning::SystemContextSource;

/// Opening tag of a reasoning block.
pub const THINK_OPEN: &str = "<think>";
/// Closing tag of a reasoning block.
pub const THINK_CLOSE: &str = "</think>";

/// The `vllm_responses` adapter.
#[derive(Debug)]
pub struct VllmResponsesAdapter {
    transport: ProviderTransport,
}

impl VllmResponsesAdapter {
    /// Adapter over the OAGW client and the S2S context source.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
    ) -> Self {
        Self {
            transport: ProviderTransport::new(gateway, system),
        }
    }
}

#[async_trait]
impl LlmProvider for VllmResponsesAdapter {
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        let body = build_request_body(&req, true);
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmFailure::provider_error(CANCELLED_MESSAGE)),
            response = self.transport.send(target, &req.model, &body, true, &[]) => response?,
        };
        let body = response.into_body().into_stream();
        Ok(event_stream(
            body,
            ResponsesTranslator::with_think_split(),
            cancel,
        ))
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure> {
        let body = build_request_body(&req, false);
        let response = self
            .transport
            .send(target, &req.model, &body, false, &[])
            .await?;
        let body = read_limited(response.into_body(), COMPLETION_BODY_LIMIT).await;
        let mut completion = parse_completion(&body)?;
        completion.text = strip_think(&completion.text);
        Ok(completion)
    }
}

/// vLLM Responses request body (module docs).
#[must_use]
pub fn build_request_body(req: &LlmRequest, stream: bool) -> Value {
    let mut body = Map::new();
    merge_extra_body(&mut body, req.api_params.extra_body.as_ref());
    insert_api_params(&mut body, &req.api_params);

    body.insert("model".to_owned(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(req.instructions));
    }
    let input: Vec<Value> = req.input.iter().map(input_item).collect();
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), json!(stream));
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("user".to_owned(), json!(req.user));
    Value::Object(body)
}

// ── <think> splitting ────────────────────────────────────────────────────

/// A piece of model output classified by [`ThinkSplitter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkSegment {
    /// Answer text.
    Text(String),
    /// Text inside a `<think>` block.
    Reasoning(String),
}

/// Incremental `<think>` … `</think>` splitter. A tail that may be the start
/// of the next tag is held back until the following delta (or
/// [`ThinkSplitter::finish`]) decides it. Whitespace at the start of the
/// answer, and right after a closing tag, is dropped.
#[derive(Debug)]
pub struct ThinkSplitter {
    in_think: bool,
    held: String,
    trim_answer_start: bool,
}

impl Default for ThinkSplitter {
    fn default() -> Self {
        Self {
            in_think: false,
            held: String::new(),
            trim_answer_start: true,
        }
    }
}

impl ThinkSplitter {
    /// Classifies `delta` (with the held tail of the previous one).
    pub fn push(&mut self, delta: &str) -> Vec<ThinkSegment> {
        let mut input = std::mem::take(&mut self.held);
        input.push_str(delta);
        let mut out = Vec::new();
        let mut rest = input.as_str();
        loop {
            let tag = if self.in_think {
                THINK_CLOSE
            } else {
                THINK_OPEN
            };
            if let Some(position) = rest.find(tag) {
                self.emit(&rest[..position], &mut out);
                rest = &rest[position + tag.len()..];
                self.in_think = !self.in_think;
                if !self.in_think {
                    self.trim_answer_start = true;
                }
                continue;
            }
            // Tags are ASCII, so the held suffix starts at a char boundary.
            let keep = partial_tag_suffix(rest, tag);
            let (ready, held) = rest.split_at(rest.len() - keep);
            self.emit(ready, &mut out);
            held.clone_into(&mut self.held);
            return out;
        }
    }

    /// Emits the held tail at the end of the output.
    pub fn finish(&mut self) -> Vec<ThinkSegment> {
        let held = std::mem::take(&mut self.held);
        let mut out = Vec::new();
        self.emit(&held, &mut out);
        out
    }

    fn emit(&mut self, text: &str, out: &mut Vec<ThinkSegment>) {
        if text.is_empty() {
            return;
        }
        if self.in_think {
            out.push(ThinkSegment::Reasoning(text.to_owned()));
            return;
        }
        let text = if self.trim_answer_start {
            let trimmed = text.trim_start();
            if trimmed.is_empty() {
                return;
            }
            self.trim_answer_start = false;
            trimmed
        } else {
            text
        };
        out.push(ThinkSegment::Text(text.to_owned()));
    }
}

/// Length of the longest proper prefix of `tag` that `text` ends with.
fn partial_tag_suffix(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|len| text.ends_with(&tag[..*len]))
        .unwrap_or(0)
}

/// `text` without its `<think>` blocks (the answer only).
#[must_use]
pub fn strip_think(text: &str) -> String {
    let mut splitter = ThinkSplitter::default();
    let mut segments = splitter.push(text);
    segments.extend(splitter.finish());
    segments
        .into_iter()
        .filter_map(|segment| match segment {
            ThinkSegment::Text(text) => Some(text),
            ThinkSegment::Reasoning(_) => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "vllm_responses_tests.rs"]
mod tests;
