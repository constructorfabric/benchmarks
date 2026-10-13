//! The Chat Completions API adapter (`openai_chat_completions`): built-in tools are dropped.

use mini_chat_sdk::TokenUsage;
use serde_json::{Map, Value, json};

use super::adapter::{Adapter, EventParser};
use super::common::{
    failure_message, insert_sampling, merge_extra_body, normalize_incomplete_reason, parse_usage,
};
use crate::domain::llm::{
    Completion, InputRole, LlmCompletion, LlmEvent, LlmRequest, ProviderError,
};

/// The Chat Completions adapter.
pub struct ChatCompletions;

impl Adapter for ChatCompletions {
    fn build_body(&self, request: &LlmRequest, stream: bool) -> Result<Value, ProviderError> {
        let mut messages = Vec::with_capacity(request.input.len() + 1);
        if !request.system_prompt.is_empty() {
            messages.push(json!({ "role": "system", "content": request.system_prompt }));
        }
        for message in &request.input {
            let role = match message.role {
                InputRole::User => "user",
                InputRole::Assistant => "assistant",
            };
            messages.push(json!({ "role": role, "content": message.text }));
        }
        let mut body = Map::new();
        body.insert("model".to_owned(), Value::from(request.model.clone()));
        body.insert("messages".to_owned(), Value::Array(messages));
        body.insert("stream".to_owned(), Value::from(stream));
        if stream {
            body.insert(
                "stream_options".to_owned(),
                json!({ "include_usage": true }),
            );
        }
        body.insert(
            "max_completion_tokens".to_owned(),
            Value::from(request.max_output_tokens),
        );
        body.insert(
            "user".to_owned(),
            Value::from(request.identity.provider_user()),
        );
        insert_sampling(&mut body, &request.api_params);
        if !request.api_params.stop.is_empty() {
            body.insert("stop".to_owned(), json!(request.api_params.stop));
        }
        if let Some(effort) = &request.api_params.reasoning_effort {
            body.insert("reasoning_effort".to_owned(), Value::from(effort.clone()));
        }
        merge_extra_body(&mut body, &request.api_params);
        Ok(Value::Object(body))
    }

    fn parser(&self) -> Box<dyn EventParser> {
        Box::new(ChatCompletionsParser::default())
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderError> {
        let text = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::provider("the provider response has no answer"))?;
        Ok(LlmCompletion {
            text: text.to_owned(),
            usage: body.get("usage").map(chat_usage),
        })
    }
}

fn chat_usage(usage: &Value) -> TokenUsage {
    parse_usage(
        usage,
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
        "completion_tokens_details",
    )
}

#[derive(Default)]
struct ChatCompletionsParser {
    id: Option<String>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
    finished: bool,
}

impl ChatCompletionsParser {
    fn completed(&mut self) -> Vec<LlmEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let completion = match self.finish_reason.as_deref() {
            Some("length") => Completion::Incomplete {
                reason: normalize_incomplete_reason(Some("length")),
            },
            Some("content_filter") => Completion::Incomplete {
                reason: normalize_incomplete_reason(Some("content_filter")),
            },
            _ => Completion::Complete,
        };
        vec![LlmEvent::Completed {
            completion,
            response_id: self.id.clone(),
            usage: self.usage,
            citations: Vec::new(),
        }]
    }
}

impl EventParser for ChatCompletionsParser {
    fn push(&mut self, _event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let data = data.trim();
        if data == "[DONE]" {
            return self.completed();
        }
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        if json.get("error").is_some() {
            self.finished = true;
            return vec![LlmEvent::Failed {
                message: failure_message(data),
                usage: None,
            }];
        }
        if self.id.is_none() {
            self.id = json.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(usage) = json.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(chat_usage(usage));
        }
        let mut out = Vec::new();
        if let Some(choice) = json.pointer("/choices/0") {
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str)
                && !text.is_empty()
            {
                out.push(LlmEvent::TextDelta(text.to_owned()));
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<LlmEvent> {
        if self.finish_reason.is_some() {
            self.completed()
        } else {
            Vec::new()
        }
    }
}
