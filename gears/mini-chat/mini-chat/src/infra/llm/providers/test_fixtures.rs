#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(clippy::needless_pass_by_value)]
//! Fixtures shared by the adapter tests of slice 7: the pieces of a request
//! build, scripted wire events and the translation of a whole scripted stream.

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, UsageTokens, WebSearchContextSize};
use serde_json::Value;
use uuid::Uuid;

use super::{EventTranslator, ProviderAdapter};
use crate::config::ProviderKind;
use crate::domain::service::quota::test_support::model;
use crate::domain::service::stream::context::{ContextPlan, CurrentMessage};
use crate::domain::service::stream::ports::{
    BuiltRequest, HistoryMessage, HistoryRole, ProviderTarget,
};
use crate::domain::service::stream::request::ProviderRequestInput;
use crate::domain::service::stream::tools::{ToolSpec, ToolsDecision};
use crate::domain::service::stream::types::{FunctionCall, ProviderEvent};
use crate::infra::llm::sse_parse::SseEvent;

pub const TENANT: Uuid = Uuid::from_u128(0x7E);
pub const USER: Uuid = Uuid::from_u128(0x55);
pub const CHAT: Uuid = Uuid::from_u128(0xC1);

/// The pieces of one request build.
pub struct Parts {
    pub target: ProviderTarget,
    pub entry: ModelCatalogEntry,
    pub plan: ContextPlan,
    pub tools: ToolsDecision,
    pub max_output: i64,
}

impl Parts {
    /// A turn on `kind` at `api_path`: model `model-test`, system prompt `SYS`,
    /// the current message `hello`, 500 output tokens, no tools.
    pub fn new(kind: ProviderKind, alias: &str, api_path: &str) -> Self {
        let mut entry = model("p", ModelTier::Premium).build();
        entry.provider_model_id = "model-test".to_owned();
        entry.system_prompt = "SYS".to_owned();
        entry.max_tool_calls = 2;
        entry.web_search_context_size = WebSearchContextSize::Medium;
        entry.max_num_results = 7;
        Self {
            target: ProviderTarget {
                kind,
                alias: alias.to_owned(),
                api_path: api_path.to_owned(),
                api_version: None,
            },
            entry,
            plan: ContextPlan {
                instructions: "SYS".to_owned(),
                current: Some(CurrentMessage {
                    content: "hello".to_owned(),
                    images: Vec::new(),
                }),
                ..ContextPlan::default()
            },
            tools: ToolsDecision {
                feature_label: "none".to_owned(),
                ..ToolsDecision::default()
            },
            max_output: 500,
        }
    }

    pub fn input(&self) -> ProviderRequestInput<'_> {
        ProviderRequestInput {
            target: &self.target,
            entry: &self.entry,
            plan: &self.plan,
            tools: &self.tools,
            max_output_tokens_applied: self.max_output,
            tenant_id: TENANT,
            user_id: USER,
            chat_id: CHAT,
        }
    }

    pub fn build_with(&self, adapter: &dyn ProviderAdapter) -> BuiltRequest {
        adapter
            .build_request(&self.input())
            .expect("the request is built")
    }

    pub fn set_tools(&mut self, tools: Vec<ToolSpec>, label: &str) {
        self.tools = ToolsDecision {
            tools,
            feature_label: label.to_owned(),
            ..ToolsDecision::default()
        };
    }
}

/// The `user` value: the two simple UUIDs, tenant first.
pub fn user_hex() -> String {
    format!("{}{}", TENANT.simple(), USER.simple())
}

pub fn history(role: HistoryRole, content: &str) -> HistoryMessage {
    HistoryMessage {
        id: Uuid::from_u128(0x4001),
        role,
        content: content.to_owned(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

pub fn file_search() -> ToolSpec {
    ToolSpec::FileSearch {
        vector_store_id: "vs_chat".to_owned(),
        max_num_results: 7,
    }
}

pub fn web_search() -> ToolSpec {
    ToolSpec::WebSearch {
        search_context_size: WebSearchContextSize::Medium,
    }
}

pub fn code_interpreter() -> ToolSpec {
    ToolSpec::CodeInterpreter {
        file_ids: vec!["file-sheet1".to_owned()],
    }
}

/// Every tool the decision can hold.
pub fn all_tools() -> Vec<ToolSpec> {
    vec![
        file_search(),
        web_search(),
        code_interpreter(),
        ToolSpec::SearchKnowledge,
    ]
}

/// A header value by case-insensitive name.
pub fn header<'a>(request: &'a BuiltRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The sorted top-level keys of a body.
pub fn keys(body: &Value) -> Vec<String> {
    let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// An SSE event with an `event:` name.
pub fn named(name: &str, data: Value) -> SseEvent {
    SseEvent {
        event: Some(name.to_owned()),
        data: data.to_string(),
        id: None,
    }
}

/// An SSE event without an `event:` line.
pub fn unnamed(data: Value) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_string(),
        id: None,
    }
}

/// An SSE event without an `event:` line and with raw `data`.
pub fn raw(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_owned(),
        id: None,
    }
}

pub fn usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: reasoning,
    }
}

/// Translates every event of a scripted body in order, then ends the body.
pub fn run_stream(translator: &mut dyn EventTranslator, events: &[SseEvent]) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    for event in events {
        out.extend(translator.translate(event));
    }
    out.extend(translator.finish());
    out
}

pub fn text_delta(text: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        text: text.to_owned(),
        item_id: None,
        content_index: None,
    }
}

pub fn call(call_id: &str, name: &str, arguments: &str) -> FunctionCall {
    FunctionCall {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// A plan with a summary and a short history for the order tests.
pub fn plan_with_history() -> ContextPlan {
    ContextPlan {
        instructions: "SYS".to_owned(),
        summary: Some(crate::domain::service::stream::context::SummaryItem {
            text: "PREAMBLE\n\nthe summary".to_owned(),
        }),
        history: vec![
            history(HistoryRole::User, "q1"),
            history(HistoryRole::Assistant, "a1"),
        ],
        current: Some(CurrentMessage {
            content: "hello".to_owned(),
            images: Vec::new(),
        }),
        ..ContextPlan::default()
    }
}

/// Two tool outputs for the continuation tests: `search_knowledge` twice.
pub fn outputs() -> Vec<crate::domain::service::stream::ports::ToolCallOutput> {
    use crate::domain::service::stream::ports::ToolCallOutput;
    vec![
        ToolCallOutput {
            call: call("call_1", "search_knowledge", "{\"query\":\"alpha\"}"),
            output: "{\"results\":[{\"text\":\"A\"}]}".to_owned(),
        },
        ToolCallOutput {
            call: call("call_2", "search_knowledge", "{\"query\":\"beta\"}"),
            output: "{\"error\":\"search limit reached\"}".to_owned(),
        },
    ]
}

/// A built request with a JSON body, for continuation tests.
pub fn request_with(kind: ProviderKind, body: Value) -> BuiltRequest {
    BuiltRequest {
        kind,
        uri: "/alias/path?api-version=1".to_owned(),
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body,
    }
}
