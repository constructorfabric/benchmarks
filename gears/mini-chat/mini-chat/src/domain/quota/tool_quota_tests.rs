//! Daily tool quota check tests: web search and code interpreter limits read from the daily
//! `total` row, the gates (request flag, attachment, tool support, kill switch), the order of
//! the two checks and the configured quotas.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check`,
//! flow `cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-tool-quota`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};

use super::check_tool_quota;
use crate::domain::error::QuotaScope;
use crate::domain::quota::estimation::RequestFacts;
use crate::domain::quota::rows::BucketRow;
use crate::test_support::quota::{default_settings, plain_facts, standard_model};

fn counters(web: i64, code: i64) -> BucketRow {
    BucketRow {
        web_search_calls: web,
        code_interpreter_calls: code,
        ..BucketRow::default()
    }
}

fn model_with_tools(web: bool, code: bool) -> ModelCatalogEntry {
    let mut model = standard_model();
    model.general_config.tool_support.web_search = web;
    model.general_config.tool_support.code_interpreter = code;
    model
}

fn web_facts() -> RequestFacts {
    RequestFacts {
        web_search_enabled: true,
        ..plain_facts()
    }
}

fn xlsx_facts() -> RequestFacts {
    RequestFacts {
        ready_xlsx_attachments: 1,
        ..plain_facts()
    }
}

fn check(
    model: &ModelCatalogEntry,
    switches: &KillSwitches,
    facts: &RequestFacts,
    row: &BucketRow,
) -> Result<(), QuotaScope> {
    check_tool_quota(model, switches, facts, row, &default_settings())
}

// ── web search ───────────────────────────────────────────────────────────────

#[test]
fn a_web_search_request_is_rejected_once_the_daily_count_reaches_the_quota() {
    let model = model_with_tools(true, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(75, 0)
        ),
        Err(QuotaScope::WebSearch)
    );
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(1_000, 0)
        ),
        Err(QuotaScope::WebSearch)
    );
}

#[test]
fn a_web_search_request_below_the_quota_is_admitted() {
    let model = model_with_tools(true, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(74, 0)
        ),
        Ok(())
    );
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &BucketRow::default()
        ),
        Ok(())
    );
}

#[test]
fn a_request_without_web_search_is_never_rejected_by_its_quota() {
    let model = model_with_tools(true, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &plain_facts(),
            &counters(10_000, 0)
        ),
        Ok(())
    );
}

#[test]
fn a_model_without_web_search_is_not_checked_even_when_the_flag_is_set() {
    let model = model_with_tools(false, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(10_000, 0)
        ),
        Ok(())
    );
}

#[test]
fn the_configured_web_search_quota_is_used() {
    let model = model_with_tools(true, true);
    let mut settings = default_settings();
    settings.web_search_daily_quota = 5;
    assert_eq!(
        check_tool_quota(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(5, 0),
            &settings
        ),
        Err(QuotaScope::WebSearch)
    );
    assert_eq!(
        check_tool_quota(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(4, 0),
            &settings
        ),
        Ok(())
    );
}

// ── code interpreter ─────────────────────────────────────────────────────────

#[test]
fn a_code_interpreter_request_is_rejected_once_the_daily_count_reaches_the_quota() {
    let model = model_with_tools(true, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &xlsx_facts(),
            &counters(0, 50)
        ),
        Err(QuotaScope::CodeInterpreter)
    );
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &xlsx_facts(),
            &counters(0, 49)
        ),
        Ok(())
    );
}

#[test]
fn a_chat_without_a_ready_xlsx_attachment_is_never_rejected_by_the_code_interpreter_quota() {
    let model = model_with_tools(true, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &plain_facts(),
            &counters(0, 5_000)
        ),
        Ok(())
    );
}

#[test]
fn a_model_without_the_code_interpreter_is_not_checked() {
    let model = model_with_tools(true, false);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &xlsx_facts(),
            &counters(0, 5_000)
        ),
        Ok(())
    );
}

#[test]
fn the_code_interpreter_kill_switch_skips_the_check() {
    let model = model_with_tools(true, true);
    let killed = KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    assert_eq!(
        check(&model, &killed, &xlsx_facts(), &counters(0, 5_000)),
        Ok(())
    );
}

#[test]
fn the_configured_code_interpreter_quota_is_used() {
    let model = model_with_tools(true, true);
    let mut settings = default_settings();
    settings.code_interpreter_daily_quota = 3;
    assert_eq!(
        check_tool_quota(
            &model,
            &KillSwitches::default(),
            &xlsx_facts(),
            &counters(0, 3),
            &settings
        ),
        Err(QuotaScope::CodeInterpreter)
    );
}

// ── order and independence ───────────────────────────────────────────────────

#[test]
fn web_search_is_checked_before_the_code_interpreter() {
    let model = model_with_tools(true, true);
    let both = RequestFacts {
        ready_xlsx_attachments: 2,
        web_search_enabled: true,
        ..plain_facts()
    };
    assert_eq!(
        check(&model, &KillSwitches::default(), &both, &counters(75, 50)),
        Err(QuotaScope::WebSearch)
    );
}

#[test]
fn an_exhausted_code_interpreter_rejects_even_when_web_search_has_room() {
    let model = model_with_tools(true, true);
    let both = RequestFacts {
        ready_xlsx_attachments: 2,
        web_search_enabled: true,
        ..plain_facts()
    };
    assert_eq!(
        check(&model, &KillSwitches::default(), &both, &counters(10, 50)),
        Err(QuotaScope::CodeInterpreter)
    );
}

#[test]
fn an_exhausted_tool_does_not_reject_a_request_that_uses_only_the_other_tool() {
    let model = model_with_tools(true, true);
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &xlsx_facts(),
            &counters(75, 0)
        ),
        Ok(())
    );
    assert_eq!(
        check(
            &model,
            &KillSwitches::default(),
            &web_facts(),
            &counters(0, 50)
        ),
        Ok(())
    );
}

#[test]
fn a_document_chat_has_no_daily_file_search_limit() {
    let model = model_with_tools(true, true);
    let docs = RequestFacts {
        ready_documents: 3,
        ..plain_facts()
    };
    assert_eq!(
        check(&model, &KillSwitches::default(), &docs, &counters(75, 50)),
        Ok(())
    );
}
