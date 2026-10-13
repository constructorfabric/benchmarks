//! Per-turn tool call limits
//! (`cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`).

use toolkit_macros::domain_model;

use crate::config::QuotaConfig;

/// A tool with a per-turn limit.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// `web_search`.
    WebSearch,
    /// `code_interpreter`.
    CodeInterpreter,
}

/// Decision of the per-turn limit.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLimit {
    /// The start count is within the limit.
    Within,
    /// The limit is exceeded: the pipeline cancels the provider stream and
    /// finalizes the turn as `failed` with `error_code`, no usage.
    Exceeded {
        /// `web_search_calls_exceeded` or `code_interpreter_calls_exceeded`.
        error_code: &'static str,
    },
}

/// Compares the number of start events of `tool` in the turn (the new one
/// included) with `quota.web_search_max_calls_per_message` or
/// `quota.code_interpreter_max_calls_per_message`: exceeded when it is greater.
#[must_use]
pub fn tool_call_limit(tool: ToolKind, start_count: u32, quota: &QuotaConfig) -> ToolLimit {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-take-limit
    let (limit, error_code) = match tool {
        ToolKind::WebSearch => (
            quota.web_search_max_calls_per_message,
            "web_search_calls_exceeded",
        ),
        ToolKind::CodeInterpreter => (
            quota.code_interpreter_max_calls_per_message,
            "code_interpreter_calls_exceeded",
        ),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-take-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-if-limit-exceeded
    if start_count > limit {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-return-exceeded
        return ToolLimit::Exceeded { error_code };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-return-exceeded
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-if-limit-exceeded
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-return-within
    ToolLimit::Within
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tool-call-limit:p1:inst-return-within
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
