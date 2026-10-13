#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: `cpt-cf-mini-chat-algo-quota-billing-tool-call-limit`.

use super::{ToolKind, ToolLimit, tool_call_limit};
use crate::config::QuotaConfig;

#[test]
fn a_third_web_search_start_exceeds_the_default_limit_of_two() {
    let quota = QuotaConfig::default();

    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 0, &quota),
        ToolLimit::Within
    );
    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 1, &quota),
        ToolLimit::Within
    );
    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 2, &quota),
        ToolLimit::Within
    );
    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 3, &quota),
        ToolLimit::Exceeded {
            error_code: "web_search_calls_exceeded"
        }
    );
    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 100, &quota),
        ToolLimit::Exceeded {
            error_code: "web_search_calls_exceeded"
        }
    );
}

#[test]
fn an_eleventh_code_interpreter_start_exceeds_the_default_limit_of_ten() {
    let quota = QuotaConfig::default();

    assert_eq!(
        tool_call_limit(ToolKind::CodeInterpreter, 10, &quota),
        ToolLimit::Within
    );
    assert_eq!(
        tool_call_limit(ToolKind::CodeInterpreter, 11, &quota),
        ToolLimit::Exceeded {
            error_code: "code_interpreter_calls_exceeded"
        }
    );
}

#[test]
fn the_limits_come_from_the_configuration_and_are_independent_per_tool() {
    let quota = QuotaConfig {
        web_search_max_calls_per_message: 5,
        code_interpreter_max_calls_per_message: 1,
        ..QuotaConfig::default()
    };

    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 5, &quota),
        ToolLimit::Within
    );
    assert_eq!(
        tool_call_limit(ToolKind::WebSearch, 6, &quota),
        ToolLimit::Exceeded {
            error_code: "web_search_calls_exceeded"
        }
    );
    assert_eq!(
        tool_call_limit(ToolKind::CodeInterpreter, 1, &quota),
        ToolLimit::Within
    );
    assert_eq!(
        tool_call_limit(ToolKind::CodeInterpreter, 2, &quota),
        ToolLimit::Exceeded {
            error_code: "code_interpreter_calls_exceeded"
        }
    );
}
