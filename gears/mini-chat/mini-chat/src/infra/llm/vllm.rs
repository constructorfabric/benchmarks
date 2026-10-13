//! vLLM Responses adapter (`vllm_responses`): Responses API without tools or
//! metadata; text inside `<think>` blocks is emitted as reasoning deltas.

use serde_json::{Map, Value, json};

use super::openai_responses::{apply_api_params, apply_extra_body, complete_text, input_items, parse_responses_event};
use super::{Adapter, LlmEvent, LlmFailure, LlmRequest, ParseState, Usage};

pub struct VllmResponses;

fn split_think(state: &mut ParseState, delta: &str, out: &mut Vec<LlmEvent>) {
    let mut rest = delta;
    while !rest.is_empty() {
        if state.in_think {
            if let Some(i) = rest.find("</think>") {
                if i > 0 {
                    out.push(LlmEvent::ReasoningDelta(rest[..i].to_owned()));
                }
                state.in_think = false;
                rest = &rest[i + "</think>".len()..];
            } else {
                out.push(LlmEvent::ReasoningDelta(rest.to_owned()));
                rest = "";
            }
        } else if let Some(i) = rest.find("<think>") {
            if i > 0 {
                out.push(LlmEvent::TextDelta(rest[..i].to_owned()));
            }
            state.in_think = true;
            rest = &rest[i + "<think>".len()..];
        } else {
            out.push(LlmEvent::TextDelta(rest.to_owned()));
            rest = "";
        }
    }
}

impl Adapter for VllmResponses {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = Map::new();
        apply_extra_body(&mut body, req);
        body.insert("model".into(), json!(req.model));
        body.insert("stream".into(), json!(req.stream));
        body.insert("instructions".into(), json!(req.instructions));
        body.insert("input".into(), Value::Array(input_items(&req.items)));
        body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
        apply_api_params(&mut body, req);
        body.insert("user".into(), json!(req.user));
        Value::Object(body)
    }

    fn parse_event(&self, state: &mut ParseState, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let events = parse_responses_event(state, event, data, false);
        let mut out = Vec::new();
        for e in events {
            match e {
                LlmEvent::TextDelta(d) => split_think(state, &d, &mut out),
                other => out.push(other),
            }
        }
        out
    }

    fn parse_complete(&self, body: &Value) -> Result<(String, Option<Usage>), LlmFailure> {
        let (text, usage) = complete_text(body)?;
        let cleaned = match (text.find("<think>"), text.find("</think>")) {
            (Some(a), Some(b)) if b > a => format!("{}{}", &text[..a], &text[b + "</think>".len()..]),
            _ => text,
        };
        Ok((cleaned, usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_split() {
        let mut st = ParseState::default();
        let mut out = Vec::new();
        split_think(&mut st, "a<think>r1", &mut out);
        split_think(&mut st, "r2</think>b", &mut out);
        assert_eq!(
            out,
            vec![
                LlmEvent::TextDelta("a".into()),
                LlmEvent::ReasoningDelta("r1".into()),
                LlmEvent::ReasoningDelta("r2".into()),
                LlmEvent::TextDelta("b".into()),
            ]
        );
    }
}
