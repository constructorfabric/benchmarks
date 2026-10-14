//! Tool event accounting and the mid-turn limits: the start counters of `web_search` and
//! `code_interpreter` with their per-message limits, the breach codes, the unlimited
//! `file_search`, the completed counts and the durable counter mapping.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `tool-accounting`; definition of done `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::tool_accounting::{StartDecision, ToolAccounting, ToolLimits};
use super::tools::ToolKind;
use crate::domain::error::StreamErrorCode;
use crate::domain::quota::ToolCallCounts;

fn accounting(web: u32, code: u32) -> ToolAccounting {
    ToolAccounting::new(ToolLimits {
        web_search: web,
        code_interpreter: code,
    })
}

#[test]
fn web_search_starts_up_to_the_limit_are_relayed_and_the_next_one_is_a_breach() {
    let mut tools = accounting(2, 10);
    assert_eq!(tools.on_start("web_search"), StartDecision::Relay);
    assert_eq!(tools.on_start("web_search"), StartDecision::Relay);
    assert_eq!(
        tools.on_start("web_search"),
        StartDecision::Breach(StreamErrorCode::WebSearchCallsExceeded)
    );
}

#[test]
fn a_limit_of_one_breaches_on_the_second_start() {
    let mut tools = accounting(1, 10);
    assert_eq!(tools.on_start("web_search"), StartDecision::Relay);
    assert_eq!(
        tools.on_start("web_search"),
        StartDecision::Breach(StreamErrorCode::WebSearchCallsExceeded)
    );
}

#[test]
fn code_interpreter_has_its_own_limit_and_code() {
    let mut tools = accounting(2, 3);
    for _ in 0..3 {
        assert_eq!(tools.on_start("code_interpreter"), StartDecision::Relay);
    }
    assert_eq!(
        tools.on_start("code_interpreter"),
        StartDecision::Breach(StreamErrorCode::CodeInterpreterCallsExceeded)
    );
}

#[test]
fn the_two_start_counters_are_independent() {
    let mut tools = accounting(1, 1);
    assert_eq!(tools.on_start("web_search"), StartDecision::Relay);
    assert_eq!(tools.on_start("code_interpreter"), StartDecision::Relay);
    assert_eq!(
        tools.on_start("code_interpreter"),
        StartDecision::Breach(StreamErrorCode::CodeInterpreterCallsExceeded)
    );
}

#[test]
fn file_search_and_function_tools_have_no_mid_turn_limit() {
    let mut tools = accounting(1, 1);
    for _ in 0..50 {
        assert_eq!(tools.on_start("file_search"), StartDecision::Relay);
        assert_eq!(tools.on_start("function_call"), StartDecision::Relay);
        assert_eq!(tools.on_start("search_knowledge"), StartDecision::Relay);
    }
}

#[test]
fn done_events_are_counted_and_name_the_durable_counter() {
    let mut tools = accounting(2, 10);
    assert_eq!(tools.on_done("web_search"), Some(ToolKind::WebSearch));
    assert_eq!(tools.on_done("web_search"), Some(ToolKind::WebSearch));
    assert_eq!(
        tools.on_done("code_interpreter"),
        Some(ToolKind::CodeInterpreter)
    );
    assert_eq!(tools.on_done("file_search"), Some(ToolKind::FileSearch));
    assert_eq!(
        tools.completed(),
        ToolCallCounts {
            web_search: 2,
            code_interpreter: 1,
            file_search: 1,
        }
    );
}

#[test]
fn a_tool_name_without_a_counter_is_not_counted() {
    let mut tools = accounting(2, 10);
    assert_eq!(tools.on_done("function_call"), None);
    assert_eq!(tools.on_done("unknown_tool"), None);
    assert_eq!(tools.completed(), ToolCallCounts::default());
}

#[test]
fn starts_alone_never_count_as_completed_calls() {
    let mut tools = accounting(2, 10);
    tools.on_start("web_search");
    tools.on_start("code_interpreter");
    assert_eq!(tools.completed(), ToolCallCounts::default());
}

#[test]
fn a_breaching_start_leaves_the_completed_counts_alone() {
    let mut tools = accounting(1, 10);
    tools.on_start("web_search");
    tools.on_done("web_search");
    assert_eq!(
        tools.on_start("web_search"),
        StartDecision::Breach(StreamErrorCode::WebSearchCallsExceeded)
    );
    assert_eq!(tools.completed().web_search, 1);
}
