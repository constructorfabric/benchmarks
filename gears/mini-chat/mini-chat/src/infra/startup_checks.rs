//! Startup checks that log and continue
//! (`cpt-cf-mini-chat-flow-gear-foundation-gear-start`, steps
//! `inst-check-summary-model` .. `inst-log-summary-model`).

use mini_chat_sdk::PolicySnapshot;
use uuid::Uuid;

use crate::infra::policy_gateway::{ModelPolicyGateway, PolicyGatewayError};

/// Summary model used when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Result of the summary-model check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryModelStatus {
    /// The model is in the catalog and enabled.
    Enabled,
    /// The model is not in the catalog.
    Missing,
    /// The model is in the catalog but disabled.
    Disabled,
    /// The snapshot could not be read (resolution failure, plugin error, timeout).
    SnapshotUnavailable,
}

/// Classifies `model_id` in the catalog of `snapshot`.
fn classify(snapshot: &PolicySnapshot, model_id: &str) -> SummaryModelStatus {
    match snapshot
        .model_catalog
        .iter()
        .find(|entry| entry.id == model_id)
    {
        Some(entry) if entry.enabled => SummaryModelStatus::Enabled,
        Some(_) => SummaryModelStatus::Disabled,
        None => SummaryModelStatus::Missing,
    }
}

/// Logs the error of a missing or disabled summary model or of an unreadable
/// snapshot; nothing for an enabled model.
fn log_status(
    model_id: &str,
    status: SummaryModelStatus,
    snapshot_error: Option<&PolicyGatewayError>,
) {
    let message = match status {
        SummaryModelStatus::Enabled => return,
        SummaryModelStatus::Missing => "thread summary model is not in the model catalog",
        SummaryModelStatus::Disabled => "thread summary model is disabled in the model catalog",
        SummaryModelStatus::SnapshotUnavailable => {
            "thread summary model check failed: policy snapshot unavailable"
        }
    };
    let cause = snapshot_error.map(ToString::to_string).unwrap_or_default();
    tracing::error!(model = model_id, %cause, "{message}");
}

/// Looks up the effective summary model in the current policy snapshot and
/// logs an error naming the model id when it is missing, disabled or the
/// snapshot is unreadable. Never fails.
pub async fn check_summary_model(
    policy: &ModelPolicyGateway,
    user_id: Uuid,
    summary_model_id: &str,
) -> SummaryModelStatus {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-check-summary-model
    let model_id = if summary_model_id.is_empty() {
        DEFAULT_SUMMARY_MODEL_ID
    } else {
        summary_model_id
    };
    let snapshot = async {
        let version = policy.current_policy_version(user_id).await?;
        policy
            .policy_snapshot(user_id, version.policy_version)
            .await
    }
    .await;
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-check-summary-model
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-if-summary-model-bad
    let status = match &snapshot {
        Ok(snapshot) => classify(snapshot, model_id),
        Err(_) => SummaryModelStatus::SnapshotUnavailable,
    };
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-log-summary-model
    log_status(model_id, status, snapshot.as_ref().err());
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-log-summary-model
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-if-summary-model-bad
    status
}

#[cfg(test)]
#[path = "startup_checks_tests.rs"]
mod tests;
