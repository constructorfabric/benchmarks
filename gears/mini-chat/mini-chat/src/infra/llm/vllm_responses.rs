//! vLLM Responses adapter (`kind: vllm_responses`): Responses wire format
//! without tools or metadata; `<think>` blocks become `reasoning` deltas.

use serde_json::{Value, json};

use super::openai_responses::{OpenAiResponses, build_input};
use super::sse_parser::SseEvent;
use super::{
    Adapter, CompletionResult, DeltaKind, LlmRequest, ProviderEvent, apply_sampling,
    merge_extra_body,
};

#[derive(Debug, Default)]
pub struct VllmResponses {
    inner: OpenAiResponses,
    in_think: bool,
}

impl VllmResponses {
    fn split_think(&mut self, text: &str) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            let (chunk, next, toggle) = match rest.find(tag) {
                Some(i) => (&rest[..i], &rest[i + tag.len()..], true),
                None => (rest, "", false),
            };
            if !chunk.is_empty() {
                out.push(ProviderEvent::Delta {
                    kind: if self.in_think { DeltaKind::Reasoning } else { DeltaKind::Text },
                    text: chunk.to_owned(),
                });
            }
            if toggle {
                self.in_think = !self.in_think;
            }
            rest = next;
        }
        out
    }
}

impl Adapter for VllmResponses {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = json!({
            "model": req.provider_model_id,
            "input": build_input(req),
            "stream": req.stream,
            "max_output_tokens": req.max_output_tokens,
            "user": req.user,
        });
        if !req.instructions.is_empty() {
            body["instructions"] = Value::String(req.instructions.clone());
        }
        apply_sampling(&mut body, &req.api_params, false);
        merge_extra_body(&mut body, &req.api_params);
        body
    }

    fn translate(&mut self, ev: &SseEvent) -> Vec<ProviderEvent> {
        let events = self.inner.translate(ev);
        let mut out = Vec::new();
        for e in events {
            match e {
                ProviderEvent::Delta {
                    kind: DeltaKind::Text,
                    text,
                } => out.extend(self.split_think(&text)),
                other => out.push(other),
            }
        }
        out
    }

    fn parse_completion(&self, body: &Value) -> Result<CompletionResult, String> {
        self.inner.parse_completion(body)
    }
}
