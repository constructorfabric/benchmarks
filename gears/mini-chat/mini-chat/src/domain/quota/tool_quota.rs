//! Daily tool quota check (`cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check`): reads
//! the call counters of the daily `total` row; the counters are incremented only at settlement.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};

use crate::domain::error::QuotaScope;
use crate::domain::quota::estimation::RequestFacts;
use crate::domain::quota::rows::BucketRow;
use crate::domain::quota::settings::QuotaSettings;

/// Web search first, code interpreter second, so the rejection scope is deterministic. A
/// request that does not use the tool, a model without the tool support and a code interpreter
/// kill switch are never rejected by that tool's quota; there is no `file_search` limit.
///
/// # Errors
///
/// [`QuotaScope::WebSearch`] or [`QuotaScope::CodeInterpreter`] when the daily count reached
/// the configured daily quota.
pub fn check_tool_quota(
    entry: &ModelCatalogEntry,
    kill_switches: &KillSwitches,
    facts: &RequestFacts,
    daily_total: &BucketRow,
    settings: &QuotaSettings,
) -> Result<(), QuotaScope> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-gating
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-read-only
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-scope
    // a request that does not use a tool, a model without its support and the code interpreter
    // kill switch are never rejected by that tool's quota; this check only reads the counters
    // (they are added at settlement) and there is no daily `file_search` limit
    let tools = &entry.general_config.tool_support;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-scope
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-read-only
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-order
    // web search first, code interpreter second, so the rejection scope is deterministic
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-web-applies
    let web_applies = facts.web_search_enabled && tools.web_search;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-web-applies
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-web-check
    if web_applies
        && reached(
            daily_total.web_search_calls,
            settings.web_search_daily_quota,
        )
    {
        return Err(QuotaScope::WebSearch);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-web-check
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-ci-applies
    let code_applies = facts.ready_xlsx_attachments > 0
        && tools.code_interpreter
        && !kill_switches.disable_code_interpreter;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-ci-applies
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-ci-check
    if code_applies
        && reached(
            daily_total.code_interpreter_calls,
            settings.code_interpreter_daily_quota,
        )
    {
        return Err(QuotaScope::CodeInterpreter);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-ci-check
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-order
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-gating
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-return
    Ok(())
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tool-quota-check:p1:inst-toolq-return
}

/// The daily call count has reached the quota.
fn reached(calls: i64, quota: u32) -> bool {
    calls >= i64::from(quota)
}

#[cfg(test)]
#[path = "tool_quota_tests.rs"]
mod tool_quota_tests;
