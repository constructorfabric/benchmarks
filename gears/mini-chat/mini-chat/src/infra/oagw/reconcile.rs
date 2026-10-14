//! OAGW provisioning reconcile task (algorithm `oagw-reconcile`): retries the deferred
//! entries until none remains or the gear stops.

use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::api::ServiceGatewayClientV1;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

use super::provisioning::{EntryOutcome, ProvisionTarget, ProvisioningError, provision_entry};

/// The wait before the first retry.
const FIRST_DELAY: Duration = Duration::from_secs(2);
/// The longest wait between two rounds.
const MAX_DELAY: Duration = Duration::from_secs(60);
/// How long entries may stay pending before one warning names them.
const WARN_AFTER: Duration = Duration::from_secs(120);

/// The wait before the retry that follows a wait of `current`: doubled, capped at 60 s.
#[must_use]
pub fn next_delay(current: Duration) -> Duration {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-backoff
    current.saturating_mul(2).min(MAX_DELAY)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-backoff
}

/// `true` when the retry of `target` leaves it pending.
fn stays_pending(
    outcome: &Result<EntryOutcome, ProvisioningError>,
    target: &ProvisionTarget,
) -> bool {
    match outcome {
        Ok(EntryOutcome::Provisioned) => {
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-recovered
            tracing::info!(provider = %target.label, "deferred provider is now provisioned");
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-recovered
            false
        }
        Ok(EntryOutcome::Deferred) => {
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-retry
            true
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-retry
        }
        Err(error) => {
            // not a secret problem any more: keep retrying until the gear stops
            tracing::debug!(%error, "retry of a deferred provider failed");
            true
        }
    }
}

/// One retry round over `pending`; returns the entries that are still pending. An entry the
/// round did not reach because of a cancellation stays pending.
async fn retry_round(
    oagw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    pending: Vec<ProvisionTarget>,
    cancel: &CancellationToken,
) -> Vec<ProvisionTarget> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-retry-entries
    let mut still_pending = Vec::with_capacity(pending.len());
    let mut remaining = pending.into_iter();
    for target in remaining.by_ref() {
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                still_pending.push(target);
                break;
            }
            outcome = provision_entry(oagw, s2s, &target) => outcome,
        };
        if stays_pending(&outcome, &target) {
            still_pending.push(target);
        }
    }
    still_pending.extend(remaining);
    still_pending
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-retry-entries
}

/// Log the one warning that names the providers still pending after 2 minutes.
fn warn_pending(pending: &[ProvisionTarget]) {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-warn
    let labels: Vec<&str> = pending.iter().map(|t| t.label.as_str()).collect();
    tracing::warn!(
        providers = %labels.join(", "),
        "OAGW provisioning is still pending after 2 minutes; retrying until the gear stops"
    );
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-warn
}

/// Retry the deferred entries: wait 2 s, retry each entry and drop those that succeed, double
/// the wait after each round that leaves entries pending (cap 60 s), and log one warning
/// naming the pending entries (their labels) once 2 minutes passed without success. Returns
/// when the set is empty or `cancel` fires; the wait ends early on cancellation.
pub async fn run_reconcile(
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: SecurityContext,
    deferred: Vec<ProvisionTarget>,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut pending = deferred;
    let mut delay = FIRST_DELAY;
    let mut warned = false;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-retry-loop
    while !pending.is_empty() && !cancel.is_cancelled() {
        // the wait ends early when the token is cancelled
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
        pending = retry_round(oagw.as_ref(), &s2s, pending, &cancel).await;
        delay = next_delay(delay);

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-warn-threshold
        if !warned && !pending.is_empty() && started.elapsed() >= WARN_AFTER {
            warned = true;
            warn_pending(&pending);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-warn-threshold
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-retry-loop

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-reconcile-return
    if !pending.is_empty() {
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-cancelled
        tracing::debug!(
            pending = pending.len(),
            "OAGW provisioning reconcile cancelled before every provider was provisioned"
        );
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-prov-state-cancelled
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-oagw-reconcile:p1:inst-prov-reconcile-return
}

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod reconcile_tests;
