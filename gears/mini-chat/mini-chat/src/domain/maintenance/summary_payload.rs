//! The thread-summary work item as the queue serializes it
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-payload`). The item is the
//! [`ThreadSummaryWork`] of the trigger of `cpt-cf-mini-chat-feature-message-streaming`.

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::domain::streaming::summary::{Frontier, THREAD_SUMMARY_UPDATE, ThreadSummaryWork};

/// Parse the payload bytes of one thread-summary message. The error text names the offending
/// field and never holds a payload value.
///
/// # Errors
///
/// The malformed reason: not a JSON object, an id that is not a UUID, a wrong
/// `system_task_type`, a timestamp that is not RFC 3339, only one half of the base frontier, or a
/// `dedupe_key` that does not start with the 32-character lowercase hex `tenant_id` and `/`.
pub fn parse_summary_payload(bytes: &[u8]) -> Result<ThreadSummaryWork, String> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-parse
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| "the payload is not JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "the payload is not a JSON object".to_owned())?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-parse
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-return-malformed
    // the malformed reason names the field and never holds a payload value
    let id = |key: &str| -> Result<Uuid, String> {
        text(key)
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .ok_or_else(|| format!("`{key}` is missing or not a UUID"))
    };
    let instant = |key: &str| -> Result<OffsetDateTime, String> {
        text(key)
            .and_then(|raw| OffsetDateTime::parse(raw, &Rfc3339).ok())
            .ok_or_else(|| format!("`{key}` is missing or not an RFC 3339 timestamp"))
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-return-malformed
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-ids
    let tenant_id = id("tenant_id")?;
    let chat_id = id("chat_id")?;
    let system_request_id = id("system_request_id")?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-ids
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-task-type
    if text("system_task_type") != Some(THREAD_SUMMARY_UPDATE) {
        return Err("`system_task_type` is not `thread_summary_update`".to_owned());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-task-type
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-target
    let frozen_target = Frontier {
        created_at: instant("frozen_target_created_at")?,
        message_id: id("frozen_target_message_id")?,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-target
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-base
    // both null is an empty base, both set is a base frontier, one alone is malformed
    let is_set = |key: &str| object.get(key).is_some_and(|value| !value.is_null());
    let base_frontier = match (
        is_set("base_frontier_created_at"),
        is_set("base_frontier_message_id"),
    ) {
        (false, false) => None,
        (true, true) => Some(Frontier {
            created_at: instant("base_frontier_created_at")?,
            message_id: id("base_frontier_message_id")?,
        }),
        _ => {
            return Err(
                "`base_frontier_created_at` and `base_frontier_message_id` must be set together"
                    .to_owned(),
            );
        }
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-base
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-dedupe
    if let Some(key) = object.get("dedupe_key").filter(|key| !key.is_null()) {
        let prefix = format!("{}/", tenant_id.simple());
        if !key.as_str().is_some_and(|key| key.starts_with(&prefix)) {
            return Err("`dedupe_key` does not start with the tenant prefix".to_owned());
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-dedupe
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-return
    // the target is taken from the payload as frozen at the enqueue and is never recomputed
    Ok(ThreadSummaryWork {
        tenant_id,
        chat_id,
        system_request_id,
        base_frontier,
        frozen_target,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-payload:p1:inst-sp-return
}

#[cfg(test)]
#[path = "summary_payload_tests.rs"]
mod summary_payload_tests;
