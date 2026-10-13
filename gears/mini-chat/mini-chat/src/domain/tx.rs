//! Retry of write transactions on transient lock contention.

use std::future::Future;
use std::time::Duration;

use crate::domain::error::DomainError;

const ATTEMPTS: u32 = 10;

/// Runs `op` (typically one `db.transaction(..)`) and re-runs it on lock contention with a short
/// exponential backoff. Every attempt is a complete, independent transaction.
///
/// # Errors
/// The last error of `op`.
pub async fn retry_contention<T, Fut>(mut op: impl FnMut() -> Fut) -> Result<T, DomainError>
where
    Fut: Future<Output = Result<T, DomainError>>,
{
    let mut delay = Duration::from_millis(5);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match op().await {
            Err(e) if e.is_contention() && attempt < ATTEMPTS => {
                tracing::debug!(attempt, error = %e, "retrying contended transaction");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            other => return other,
        }
    }
}
