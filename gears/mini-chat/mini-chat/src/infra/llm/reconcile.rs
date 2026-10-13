//! Deferred provider reconciliation
//! (`cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

use crate::infra::llm::oagw_provisioning::ProvisionTarget;

/// First retry interval.
pub const RECONCILE_INITIAL_INTERVAL: Duration = Duration::from_secs(2);
/// Retry interval cap.
pub const RECONCILE_MAX_INTERVAL: Duration = Duration::from_secs(60);
/// Pending time after which one warning is logged.
pub const RECONCILE_WARN_AFTER: Duration = Duration::from_secs(120);

/// Backoff schedule of the reconcile task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileSchedule {
    /// Initial wait and first interval.
    pub initial_interval: Duration,
    /// Interval cap.
    pub max_interval: Duration,
    /// Time since the start after which a still-pending set is warned about once.
    pub warn_after: Duration,
}

impl Default for ReconcileSchedule {
    fn default() -> Self {
        Self {
            initial_interval: RECONCILE_INITIAL_INTERVAL,
            max_interval: RECONCILE_MAX_INTERVAL,
            warn_after: RECONCILE_WARN_AFTER,
        }
    }
}

/// Why a retry of a deferred target failed (logged, never contains a secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryFailure {
    /// Failure cause.
    pub cause: String,
}

/// Retries the provisioning of one deferred target.
#[async_trait]
pub trait TargetRetry: Send + Sync {
    /// Retries upstream, chat route and RAG routes of `target`.
    ///
    /// # Errors
    ///
    /// [`RetryFailure`] when the target stays pending.
    async fn retry(
        &self,
        ctx: &SecurityContext,
        target: &ProvisionTarget,
    ) -> Result<(), RetryFailure>;
}

/// Result of one pass over the pending targets.
enum Round {
    /// Every target was tried; these are still pending.
    Finished(Vec<ProvisionTarget>),
    /// The gear is stopping; these are still pending.
    Cancelled(Vec<ProvisionTarget>),
}

/// Logs the outcome of one retry below WARN level (the single warning is
/// [`warn_pending`]).
fn log_attempt(target: &ProvisionTarget, failure: Option<&RetryFailure>) {
    match failure {
        None => tracing::info!(provider = %target.provider_id, "deferred provider provisioned"),
        Some(failure) => tracing::info!(
            provider = %target.provider_id,
            cause = %failure.cause,
            "deferred provider still pending"
        ),
    }
}

/// Tries every pending target once.
async fn retry_round(
    retry: &dyn TargetRetry,
    ctx: &SecurityContext,
    pending: Vec<ProvisionTarget>,
    cancel: &CancellationToken,
) -> Round {
    let mut still_pending = Vec::new();
    let mut targets = pending.into_iter();
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-for-each-deferred
    while let Some(target) = targets.next() {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-retry-target
        let attempt = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = retry.retry(ctx, &target) => Some(result),
        };
        match attempt {
            None => {
                still_pending.push(target);
                still_pending.extend(targets);
                return Round::Cancelled(still_pending);
            }
            Some(Ok(())) => log_attempt(&target, None),
            Some(Err(failure)) => {
                log_attempt(&target, Some(&failure));
                still_pending.push(target);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-retry-target
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-for-each-deferred
    Round::Finished(still_pending)
}

/// Logs the single warning naming the providers that are still pending.
fn warn_pending(pending: &[ProvisionTarget]) {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-warn-pending
    let names: Vec<&str> = pending.iter().map(|t| t.provider_id.as_str()).collect();
    tracing::warn!(providers = %names.join(", "), "providers are still pending after the warning period");
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-warn-pending
}

/// Retries the deferred targets until none is pending or `cancel` fires and
/// returns the targets still pending.
pub async fn reconcile_providers(
    retry: Arc<dyn TargetRetry>,
    ctx: SecurityContext,
    mut pending: Vec<ProvisionTarget>,
    cancel: CancellationToken,
    schedule: ReconcileSchedule,
) -> Vec<ProvisionTarget> {
    if pending.is_empty() {
        return pending;
    }
    let started = tokio::time::Instant::now();
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-initial-delay
    let mut interval = schedule.initial_interval;
    let mut warned = false;
    if !wait(&cancel, interval).await {
        return pending;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-initial-delay
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-while-pending
    while !pending.is_empty() {
        pending = match retry_round(retry.as_ref(), &ctx, pending, &cancel).await {
            Round::Cancelled(rest) => return rest,
            Round::Finished(rest) => rest,
        };
        if pending.is_empty() {
            break;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-if-warn-due
        if !warned && started.elapsed() >= schedule.warn_after {
            warned = true;
            warn_pending(&pending);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-if-warn-due
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-backoff
        if !wait(&cancel, interval).await {
            return pending;
        }
        interval = (interval * 2).min(schedule.max_interval);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-backoff
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-while-pending
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-return-reconcile
    pending
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-reconcile-providers:p1:inst-return-reconcile
}

/// Waits `interval`; `false` when cancelled first.
async fn wait(cancel: &CancellationToken, interval: Duration) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(interval) => true,
    }
}

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod tests;
