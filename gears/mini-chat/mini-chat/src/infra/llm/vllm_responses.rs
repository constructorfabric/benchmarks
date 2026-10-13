//! vLLM Responses adapter (`provider kind = vllm_responses`): the Responses protocol without
//! tools and without `metadata`; text inside `<think>…</think>` is model reasoning and is
//! streamed as [`ProviderEvent::ReasoningDelta`] (a non-streaming answer drops it).

use std::sync::Arc;

use async_trait::async_trait;
use oagw_sdk::ServiceGatewayClientV1;
use oagw_sdk::sse::ServerEvent;
use serde_json::Value;

use super::openai_responses::{Translator, build_body, parse_completion};
use super::transport::{self, Translate};
use super::types::{
    ChatAdapter, CompletionResult, ProviderError, ProviderEvent, ProviderEventStream,
    ProviderRequest,
};
use super::{ChatTarget, S2sContext};

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Body keys of the Responses request that vLLM does not get.
const DROPPED_KEYS: &[&str] = &["tools", "metadata", "max_tool_calls", "include"];

/// The vLLM Responses protocol over the in-process OAGW client.
pub struct VllmResponsesAdapter {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: S2sContext,
}

impl VllmResponsesAdapter {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: S2sContext) -> Self {
        Self { gateway, s2s }
    }

    async fn send(
        &self,
        target: &ChatTarget,
        req: &ProviderRequest,
        stream: bool,
    ) -> Result<http::Response<oagw_sdk::Body>, ProviderError> {
        let mut body = build_body(req, stream);
        if let Value::Object(map) = &mut body {
            for key in DROPPED_KEYS {
                map.remove(*key);
            }
        }
        transport::post_json(
            &self.gateway,
            &self.s2s,
            target,
            &req.model,
            &body,
            stream,
            &[],
        )
        .await
    }
}

#[async_trait]
impl ChatAdapter for VllmResponsesAdapter {
    async fn stream(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        let resp = self.send(target, &req, true).await?;
        let events = transport::event_stream(resp)?;
        Ok(transport::translate_stream(
            events,
            ThinkTranslator {
                inner: Translator::default(),
                split: ThinkSplitter::default(),
            },
        ))
    }

    async fn complete(
        &self,
        target: &ChatTarget,
        req: ProviderRequest,
    ) -> Result<CompletionResult, ProviderError> {
        let resp = self.send(target, &req, false).await?;
        let mut result = parse_completion(&transport::json_body(resp).await?)?;
        let mut split = ThinkSplitter::default();
        let mut parts = split.push(&result.text);
        parts.extend(split.flush());
        let text: String = parts
            .into_iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect();
        text.trim_start().clone_into(&mut result.text);
        Ok(result)
    }
}

/// The Responses translation with every text delta passed through a [`ThinkSplitter`].
struct ThinkTranslator {
    inner: Translator,
    split: ThinkSplitter,
}

impl Translate for ThinkTranslator {
    fn translate(&mut self, raw: &ServerEvent) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        for event in self.inner.translate(raw) {
            match event {
                ProviderEvent::TextDelta(text) => out.extend(self.split.push(&text)),
                terminal if terminal.is_terminal() => {
                    out.extend(self.split.flush());
                    out.push(terminal);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<ProviderEvent> {
        let mut out = self.split.flush();
        out.extend(self.inner.finish());
        out
    }
}

/// Splits streamed text at `<think>` / `</think>` tags, also when a tag spans several deltas:
/// a trailing prefix of the next tag is held back until the following delta (or the end).
#[derive(Default)]
struct ThinkSplitter {
    in_think: bool,
    held: String,
}

impl ThinkSplitter {
    fn push(&mut self, delta: &str) -> Vec<ProviderEvent> {
        let mut buf = std::mem::take(&mut self.held);
        buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            let tag = self.tag();
            if let Some(pos) = buf.find(tag) {
                self.emit(&buf[..pos], &mut out);
                self.in_think = !self.in_think;
                buf.drain(..pos + tag.len());
                continue;
            }
            let held = (1..tag.len())
                .rev()
                .find(|&k| buf.ends_with(&tag[..k]))
                .unwrap_or(0);
            let cut = buf.len() - held;
            self.emit(&buf[..cut], &mut out);
            buf[cut..].clone_into(&mut self.held);
            return out;
        }
    }

    /// The held-back text (an unfinished tag is just text).
    fn flush(&mut self) -> Vec<ProviderEvent> {
        let held = std::mem::take(&mut self.held);
        let mut out = Vec::new();
        self.emit(&held, &mut out);
        out
    }

    fn tag(&self) -> &'static str {
        if self.in_think {
            THINK_CLOSE
        } else {
            THINK_OPEN
        }
    }

    fn emit(&self, text: &str, out: &mut Vec<ProviderEvent>) {
        if text.is_empty() {
            return;
        }
        out.push(if self.in_think {
            ProviderEvent::ReasoningDelta(text.to_owned())
        } else {
            ProviderEvent::TextDelta(text.to_owned())
        });
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use http::Method;
    use serde_json::json;

    use super::*;
    use crate::config::ProviderKind;
    use crate::infra::llm::{ProviderEvent, ToolSpec};
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::fixtures::{chat_target, provider_request};
    use crate::test_support::gateway::{FakeGateway, Responder, SseScript};

    const PATH: &str = "/v1/responses";

    fn setup() -> (Arc<FakeGateway>, VllmResponsesAdapter) {
        let gateway = Arc::new(FakeGateway::new());
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        let adapter = VllmResponsesAdapter::new(Arc::clone(&gateway) as _, s2s);
        (gateway, adapter)
    }

    fn target() -> ChatTarget {
        chat_target(ProviderKind::VllmResponses)
    }

    fn delta(text: &str) -> SseScript {
        SseScript::event(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": text}),
        )
    }

    fn completed() -> SseScript {
        SseScript::event(
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_v", "usage": {"input_tokens": 3, "output_tokens": 4}}}),
        )
    }

    async fn events_for(deltas: &[&str]) -> Vec<ProviderEvent> {
        let (gateway, adapter) = setup();
        let mut script: Vec<SseScript> = deltas.iter().map(|d| delta(d)).collect();
        script.push(completed());
        gateway.on(Method::POST, PATH, Responder::Sse(script));
        adapter
            .stream(&target(), provider_request())
            .await
            .expect("stream starts")
            .collect()
            .await
    }

    fn reasoning(s: &str) -> ProviderEvent {
        ProviderEvent::ReasoningDelta(s.to_owned())
    }

    fn text(s: &str) -> ProviderEvent {
        ProviderEvent::TextDelta(s.to_owned())
    }

    /// The streamed deltas without the terminal event, adjacent same-kind deltas merged.
    fn merged(events: &[ProviderEvent]) -> Vec<ProviderEvent> {
        let mut out: Vec<ProviderEvent> = Vec::new();
        for event in events.iter().filter(|e| !e.is_terminal()) {
            match (out.last_mut(), event) {
                (Some(ProviderEvent::TextDelta(a)), ProviderEvent::TextDelta(b))
                | (Some(ProviderEvent::ReasoningDelta(a)), ProviderEvent::ReasoningDelta(b)) => {
                    a.push_str(b);
                }
                _ => out.push(event.clone()),
            }
        }
        out
    }

    #[tokio::test]
    async fn vllm_reasoning_split() {
        let events = events_for(&["<think>a</think>b"]).await;
        assert_eq!(&events[..2], &[reasoning("a"), text("b")]);
        assert!(
            matches!(&events[2], ProviderEvent::Completed { response_id, usage: Some(u), .. }
                if response_id.as_deref() == Some("resp_v") && u.output_tokens == 4),
            "{events:?}"
        );
        assert_eq!(events.len(), 3);

        // tags split across deltas
        let events = events_for(&["<th", "ink>rea", "son</thi", "nk>ans", "wer"]).await;
        assert_eq!(merged(&events), vec![reasoning("reason"), text("answer")]);

        // no think block: plain text, a lone `<` is text
        let events = events_for(&["a < b", " ok"]).await;
        assert_eq!(merged(&events), vec![text("a < b ok")]);

        // an unterminated tag prefix at the end is flushed as text before the terminal event
        let events = events_for(&["x<thi"]).await;
        assert_eq!(merged(&events), vec![text("x<thi")]);
        assert!(events.last().unwrap().is_terminal());
    }

    #[tokio::test]
    async fn vllm_reasoning_split_body_has_no_tools_or_metadata() {
        let (gateway, adapter) = setup();
        gateway.on(Method::POST, PATH, Responder::Sse(vec![completed()]));
        let mut req = provider_request();
        req.tools = vec![ToolSpec::Function {
            name: "search_knowledge".to_owned(),
            description: "d".to_owned(),
            parameters: json!({"type": "object"}),
        }];
        let user = req.user.clone();
        let _ = adapter
            .stream(&target(), req)
            .await
            .expect("stream")
            .count()
            .await;
        let body = gateway.requests()[0].json.clone().expect("json body");
        for absent in ["tools", "metadata", "max_tool_calls", "include"] {
            assert!(body.get(absent).is_none(), "{absent} in {body}");
        }
        assert_eq!(body["user"], user);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(body["max_output_tokens"], 1024);
    }

    #[tokio::test]
    async fn vllm_reasoning_split_complete_drops_think_blocks() {
        let (gateway, adapter) = setup();
        gateway.on(
            Method::POST,
            PATH,
            Responder::json(
                200,
                json!({"output": [{"type": "message", "content": [
                    {"type": "output_text", "text": "<think>hmm</think>The summary"}]}],
                    "usage": {"input_tokens": 1, "output_tokens": 2}}),
            ),
        );
        let mut req = provider_request();
        req.stream = false;
        let result = adapter.complete(&target(), req).await.expect("complete");
        assert_eq!(result.text, "The summary");
        assert_eq!(result.usage.map(|u| u.output_tokens), Some(2));
    }
}
