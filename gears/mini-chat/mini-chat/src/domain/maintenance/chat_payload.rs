//! The chat-cleanup message as the queue serializes it
//! (`cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload`). The handler uses only
//! `tenant_id` and `chat_id` to load the current rows, never a row list from the payload.

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The parsed chat-cleanup message.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCleanupWork {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub chat_deleted_at: OffsetDateTime,
}

/// Parse the payload bytes of one chat-cleanup message. The error text names the offending field
/// and never holds a payload value.
///
/// # Errors
///
/// The malformed reason: not a JSON object, an id that is not a UUID, `chat_deleted_at` that is
/// not RFC 3339, a `reason` other than `chat_soft_delete`, or a `dedupe_key` that does not start
/// with the 32-character lowercase hex `tenant_id` and `/`.
pub fn parse_chat_cleanup_payload(bytes: &[u8]) -> Result<ChatCleanupWork, String> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-parse
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| "the payload is not JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "the payload is not a JSON object".to_owned())?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-parse
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-return-malformed
    // the malformed reason names the field and never holds a payload value
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    let id = |key: &str| -> Result<Uuid, String> {
        text(key)
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .ok_or_else(|| format!("`{key}` is missing or not a UUID"))
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-return-malformed
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-fields
    let tenant_id = id("tenant_id")?;
    let chat_id = id("chat_id")?;
    let system_request_id = id("system_request_id")?;
    let chat_deleted_at = text("chat_deleted_at")
        .and_then(|raw| OffsetDateTime::parse(raw, &Rfc3339).ok())
        .ok_or_else(|| "`chat_deleted_at` is missing or not an RFC 3339 timestamp".to_owned())?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-reason
    if text("reason") != Some("chat_soft_delete") {
        return Err("`reason` is not `chat_soft_delete`".to_owned());
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-reason
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-dedupe
    if let Some(key) = object.get("dedupe_key").filter(|key| !key.is_null()) {
        let prefix = format!("{}/", tenant_id.simple());
        if !key.as_str().is_some_and(|key| key.starts_with(&prefix)) {
            return Err("`dedupe_key` does not start with the tenant prefix".to_owned());
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-dedupe
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-return
    // the handler uses only `tenant_id` and `chat_id` to load the current rows, never a row list
    // from the payload
    Ok(ChatCleanupWork {
        tenant_id,
        chat_id,
        system_request_id,
        chat_deleted_at,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload:p1:inst-cp-return
}

#[cfg(test)]
#[path = "chat_payload_tests.rs"]
mod chat_payload_tests;
