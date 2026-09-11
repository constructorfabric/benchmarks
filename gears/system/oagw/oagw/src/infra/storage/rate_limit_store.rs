//! In-memory token buckets (ADR 0003).

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dashmap::DashMap;

use crate::domain::error::{DomainError, RateLimitSnapshot};
use crate::domain::ratelimit::TokenBucket;
use crate::domain::repo::{RateKey, RateLimitOutcome, RateLimitStore};

/// In-memory token-bucket store.
///
/// One bucket per [`RateKey`]; buckets are created lazily and start full.
#[derive(Debug, Default)]
pub struct InMemoryRateLimitStore {
    buckets: DashMap<RateKey, TokenBucket>,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl RateLimitStore for InMemoryRateLimitStore {
    async fn try_take(
        &self,
        key: RateKey,
        capacity: u64,
        refill_rate: f64,
        cost: u64,
    ) -> Result<RateLimitOutcome, DomainError> {
        if self.buckets.get(&key).is_none() {
            self.buckets
                .entry(key.clone())
                .or_insert_with(|| TokenBucket::new(capacity, refill_rate));
        }
        let mut entry = self
            .buckets
            .get_mut(&key)
            .ok_or_else(|| DomainError::Internal { diagnostic: "bucket vanished".into() })?;
        let bucket = entry.value_mut();
        bucket.refill();
        let now = now_epoch();
        let snapshot = match bucket.try_acquire(cost) {
            Some(remaining) => {
                let secs = if refill_rate > 0.0 { 1.0 / refill_rate } else { 1.0 };
                RateLimitSnapshot {
                    limit: capacity,
                    remaining,
                    reset: now + secs.ceil() as i64,
                    retry_after: 0,
                }
            }
            None => {
                let secs = if refill_rate > 0.0 {
                    (cost.max(1) as f64 - bucket.tokens) / refill_rate
                } else {
                    1.0
                };
                RateLimitSnapshot {
                    limit: capacity,
                    remaining: 0,
                    reset: now + secs.ceil() as i64,
                    retry_after: secs.ceil().max(1.0) as u64,
                }
            }
        };
        Ok(if snapshot.remaining == 0 && snapshot.retry_after > 0 {
            RateLimitOutcome::Exceeded(snapshot)
        } else {
            RateLimitOutcome::Acquired(snapshot)
        })
    }

    async fn clear(&self) -> Result<(), DomainError> {
        self.buckets.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scope: &str) -> RateKey {
        RateKey { bucket: "b".into(), scope: scope.into() }
    }

    #[tokio::test]
    async fn buckets_are_created_per_scope_key() {
        let store = InMemoryRateLimitStore::default();
        let first = store.try_take(key("t1"), 1, 0.0, 1).await.unwrap();
        assert!(first.is_acquired());
        assert_eq!(first.snapshot().remaining, 0);

        let second = store.try_take(key("t1"), 1, 0.0, 1).await.unwrap();
        assert!(!second.is_acquired());
        assert_eq!(second.snapshot().retry_after, 1);
        assert_eq!(second.snapshot().limit, 1);
    }

    #[tokio::test]
    async fn different_scope_keys_have_independent_buckets() {
        let store = InMemoryRateLimitStore::default();
        assert!(store.try_take(key("a"), 1, 0.0, 1).await.unwrap().is_acquired());
        assert!(store.try_take(key("b"), 1, 0.0, 1).await.unwrap().is_acquired());
    }

    #[tokio::test]
    async fn clear_drops_every_bucket() {
        let store = InMemoryRateLimitStore::default();
        assert!(store.try_take(key("a"), 1, 0.0, 1).await.unwrap().is_acquired());
        store.clear().await.unwrap();
        assert!(store.try_take(key("a"), 1, 0.0, 1).await.unwrap().is_acquired());
    }
}
