//! Tool event accounting and the mid-turn limits (slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`, algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-tool-accounting`): the per-turn counters of one
//! provider task.

use toolkit_macros::domain_model;

use crate::domain::error::StreamErrorCode;
use crate::domain::quota::ToolCallCounts;
use crate::domain::streaming::tools::ToolKind;

/// `quota.web_search_max_calls_per_message` and `quota.code_interpreter_max_calls_per_message`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolLimits {
    pub web_search: u32,
    pub code_interpreter: u32,
}

/// What a `tool` `start` event leads to.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDecision {
    /// Relay the event.
    Relay,
    /// The start exceeds a per-message limit: do not relay it, cancel the provider stream and
    /// finalize `failed` with the code (no usage, estimated settlement).
    Breach(StreamErrorCode),
}

/// The counters of one turn.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ToolAccounting {
    limits: ToolLimits,
    web_search_starts: u32,
    code_interpreter_starts: u32,
    completed: ToolCallCounts,
}

impl ToolAccounting {
    /// Counters at zero.
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            limits,
            web_search_starts: 0,
            code_interpreter_starts: 0,
            completed: ToolCallCounts::default(),
        }
    }

    /// A `tool` `start` event named `name`: `web_search` and `code_interpreter` starts are
    /// counted and a start above the limit is a breach (`web_search_calls_exceeded`,
    /// `code_interpreter_calls_exceeded`); every other name, `file_search` included, is relayed
    /// without a limit.
    pub fn on_start(&mut self, name: &str) -> StartDecision {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-web-start
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-web-limit
        if name == ToolKind::WebSearch.as_str() {
            self.web_search_starts = self.web_search_starts.saturating_add(1);
            if self.web_search_starts > self.limits.web_search {
                return StartDecision::Breach(StreamErrorCode::WebSearchCallsExceeded);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-web-limit
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-web-start
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-ci-start
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-ci-limit
        if name == ToolKind::CodeInterpreter.as_str() {
            self.code_interpreter_starts = self.code_interpreter_starts.saturating_add(1);
            if self.code_interpreter_starts > self.limits.code_interpreter {
                return StartDecision::Breach(StreamErrorCode::CodeInterpreterCallsExceeded);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-ci-limit
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-ci-start
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-no-fs-limit
        // `file_search` calls are bounded only by `max_tool_calls` and the reserve
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-return
        // the relayed event (a breach returned above)
        StartDecision::Relay
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-return
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-no-fs-limit
    }

    /// A `tool` `done` event named `name`: increments the in-memory completed count and returns
    /// the tool whose durable counter must be incremented (`None` for a name without a
    /// counter).
    pub fn on_done(&mut self, name: &str) -> Option<ToolKind> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-memory-count
        let kind = [
            ToolKind::WebSearch,
            ToolKind::CodeInterpreter,
            ToolKind::FileSearch,
            ToolKind::SearchKnowledge,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == name)?;
        // the in-memory file search count also counts `search_knowledge` calls
        let counter = match kind {
            ToolKind::WebSearch => &mut self.completed.web_search,
            ToolKind::CodeInterpreter => &mut self.completed.code_interpreter,
            ToolKind::FileSearch | ToolKind::SearchKnowledge => &mut self.completed.file_search,
        };
        *counter = counter.saturating_add(1);
        Some(kind)
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-memory-count
    }

    /// The in-memory completed counts, settled at finalization.
    #[must_use]
    pub fn completed(&self) -> ToolCallCounts {
        self.completed
    }
}
