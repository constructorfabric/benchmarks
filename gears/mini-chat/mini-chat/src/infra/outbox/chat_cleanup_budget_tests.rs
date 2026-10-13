#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the delivery budget of the chat cleanup handler under the injectable
//! monotonic clock (`CHAT_CLEANUP_DELIVERY_BUDGET`, `PROVIDER_DELETE_TIMEOUT`,
//! `CHAT_CLEANUP_CONCURRENCY`; `inst-delivery-budget`, `inst-if-cut-by-deadline`,
//! `inst-if-no-time-for-store`): a fake storage client sleeps on a manual clock, so
//! the deadline, deferral and concurrency paths are exercised without a real
//! multi-second wait and without relying on paused-time auto-advance.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::service::attachments::model::StorageTarget;
use crate::domain::service::attachments::ports::{
    FileUpload, IndexStatus, StorageClient, StorageError,
};
use crate::domain::service::workers::clock::WorkerClock;
use crate::domain::service::workers::metrics::CleanupResource::File;
use crate::domain::service::workers::test_support::{ManualClock, Metric};
use crate::domain::service::workers::{
    CHAT_CLEANUP_CONCURRENCY, CHAT_CLEANUP_DELIVERY_BUDGET, HandlerOutcome,
};
use crate::test_doubles::USER_TENANT;
use crate::worker_test_env::WorkerEnv;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// One call the fake storage received.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    kind: &'static str,
    id: String,
    started_at: Duration,
}

/// A storage client whose file deletes sleep `delay` on the manual clock.
struct ClockedStorage {
    clock: Arc<ManualClock>,
    delay: Duration,
    calls: Mutex<Vec<Call>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl ClockedStorage {
    fn new(clock: &Arc<ManualClock>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            clock: Arc::clone(clock),
            delay,
            calls: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        })
    }

    fn calls(&self, kind: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.kind == kind)
            .cloned()
            .collect()
    }

    fn record(&self, kind: &'static str, id: &str) {
        self.calls.lock().unwrap().push(Call {
            kind,
            id: id.to_owned(),
            started_at: self.clock.now(),
        });
    }
}

#[async_trait]
impl StorageClient for ClockedStorage {
    async fn upload_file(
        &self,
        _: &SecurityContext,
        _: &StorageTarget,
        _: &FileUpload,
    ) -> Result<String, StorageError> {
        unimplemented!("the cleanup handler never uploads")
    }

    async fn upload_secondary(
        &self,
        _: &SecurityContext,
        _: &str,
        _: &FileUpload,
    ) -> Result<String, StorageError> {
        unimplemented!("the cleanup handler never uploads")
    }

    async fn create_vector_store(
        &self,
        _: &SecurityContext,
        _: &StorageTarget,
        _: Uuid,
    ) -> Result<String, StorageError> {
        unimplemented!("the cleanup handler never creates")
    }

    async fn add_file(
        &self,
        _: &SecurityContext,
        _: &StorageTarget,
        _: &str,
        _: &str,
        _: Uuid,
    ) -> Result<IndexStatus, StorageError> {
        unimplemented!("the cleanup handler never indexes")
    }

    async fn indexing_status(
        &self,
        _: &SecurityContext,
        _: &StorageTarget,
        _: &str,
        _: &str,
    ) -> Result<IndexStatus, StorageError> {
        unimplemented!("the cleanup handler never polls")
    }

    async fn delete_file(
        &self,
        _: &SecurityContext,
        _: &StorageTarget,
        file: &str,
    ) -> Result<(), StorageError> {
        self.record("file", file);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        self.clock.sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete_vector_store(
        &self,
        _: &SecurityContext,
        _: &StorageTarget,
        store: &str,
    ) -> Result<(), StorageError> {
        self.record("store", store);
        Ok(())
    }

    async fn delete_secondary(
        &self,
        _: &SecurityContext,
        _: &str,
        id: &str,
    ) -> Result<(), StorageError> {
        self.record("secondary", id);
        Ok(())
    }
}

/// A deleted chat with `n` owed documents and a bound store.
async fn deleted_chat(env: &WorkerEnv, n: u128) -> Uuid {
    let chat = env.m.stream.seed_chat(50, "m-oa").await;
    for k in 1..=n {
        env.owed(chat, k).await;
    }
    env.store_row(chat, Some("vs_1"), "openai").await;
    env.soft_delete_chat(chat).await;
    chat
}

async fn pending(env: &WorkerEnv, chat: Uuid) -> usize {
    let mut pending = 0;
    for id in env.att_ids_of(chat).await {
        if env.att(id).await.cleanup_status.as_deref() == Some("pending") {
            pending += 1;
        }
    }
    pending
}

#[test]
fn the_budget_and_the_concurrency_are_the_documented_ones() {
    assert_eq!(CHAT_CLEANUP_DELIVERY_BUDGET, secs(25));
    assert_eq!(CHAT_CLEANUP_CONCURRENCY, 4);
}

#[tokio::test]
async fn thirty_slow_attachments_need_four_deliveries_and_the_store_waits_for_the_last_row() {
    let env = WorkerEnv::new().await; // cleanup_worker.max_attempts 5
    let chat = deleted_chat(&env, 30).await;
    let clock = ManualClock::new();
    let storage = ClockedStorage::new(&clock, secs(8));
    let service = Arc::new(env.chat_cleanup_service_with(
        Arc::clone(&storage) as Arc<dyn StorageClient>,
        Arc::clone(&clock) as Arc<dyn WorkerClock>,
    ));

    // ---- delivery 1: calls start at 0 s, 8 s and 16 s; the one at 24 s is cut at 25 s ----
    let delivery = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.handle(USER_TENANT, chat, 0).await })
    };
    for expected in [8, 16, 24, 25] {
        // four calls: each has its own sleep and its own bound
        let at = clock.advance_when_pending(8).await;
        assert_eq!(at, secs(expected), "the next deadline of delivery 1");
    }
    let outcome = tokio::time::timeout(secs(10), delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, HandlerOutcome::Retry, "rows are still pending");
    let started: Vec<u64> = storage
        .calls("file")
        .iter()
        .map(|c| c.started_at.as_secs())
        .collect();
    assert_eq!(
        started.len(),
        16,
        "12 finished rows and 4 cut ones: {started:?}"
    );
    assert!(
        started.iter().all(|at| *at <= 24),
        "no call starts later than 25 s after the delivery start: {started:?}"
    );
    assert_eq!(pending(&env, chat).await, 18, "12 of 30 are done");
    assert!(
        storage.calls("store").is_empty(),
        "no store call while a row is pending"
    );
    assert_eq!(
        env.metrics.count(Metric::CleanupRetry(File)),
        0,
        "a cut call is not a failed attempt"
    );
    assert_eq!(env.metrics.count(Metric::CleanupFailed(File)), 0);
    for id in env.att_ids_of(chat).await {
        assert_eq!(
            env.att(id).await.cleanup_attempts,
            0,
            "rows cut or not reached keep their attempts"
        );
    }

    // ---- delivery 2 starts at 25 s: the 18 remaining rows, 12 more done ----
    let delivery = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.handle(USER_TENANT, chat, 1).await })
    };
    for expected in [33, 41, 49, 50] {
        assert_eq!(clock.advance_when_pending(8).await, secs(expected));
    }
    let outcome = tokio::time::timeout(secs(10), delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, HandlerOutcome::Retry);
    assert_eq!(pending(&env, chat).await, 6);
    assert!(storage.calls("store").is_empty());

    // ---- delivery 3 starts at 50 s: 6 rows (4 at 0 s, 2 at 8 s) end at 16 s with 9 s left ----
    let delivery = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.handle(USER_TENANT, chat, 2).await })
    };
    assert_eq!(clock.advance_when_pending(8).await, secs(58));
    assert_eq!(clock.advance_when_pending(4).await, secs(66));
    let outcome = tokio::time::timeout(secs(10), delivery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome,
        HandlerOutcome::Retry,
        "9 s are left, below PROVIDER_DELETE_TIMEOUT: no store call"
    );
    assert_eq!(pending(&env, chat).await, 0, "every row is done");
    assert!(
        storage.calls("store").is_empty(),
        "inst-if-no-time-for-store"
    );

    // ---- delivery 4 makes the store call ----
    let outcome = service.handle(USER_TENANT, chat, 3).await;
    assert_eq!(outcome, HandlerOutcome::Ok);
    let stores = storage.calls("store");
    assert_eq!(
        stores.len(),
        1,
        "the store delete is called once, after every row is done"
    );
    assert_eq!(stores[0].id, "vs_1");
    assert!(env.stores().await.is_empty());
    assert_eq!(
        storage.calls("file").len(),
        30 + 4 + 4,
        "30 deletes plus the cut calls of deliveries 1 and 2"
    );
    assert_eq!(env.metrics.count(Metric::CleanupCompleted(File)), 30);
}

#[tokio::test]
async fn at_most_four_attachments_are_in_flight_at_a_time() {
    let env = WorkerEnv::new().await;
    let chat = deleted_chat(&env, 6).await;
    let clock = ManualClock::new();
    let storage = ClockedStorage::new(&clock, secs(8));
    let service = Arc::new(env.chat_cleanup_service_with(
        Arc::clone(&storage) as Arc<dyn StorageClient>,
        Arc::clone(&clock) as Arc<dyn WorkerClock>,
    ));
    let delivery = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.handle(USER_TENANT, chat, 0).await })
    };

    assert_eq!(clock.advance_when_pending(8).await, secs(8));
    assert_eq!(clock.advance_when_pending(4).await, secs(16));
    let outcome = tokio::time::timeout(secs(10), delivery)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(storage.max_in_flight.load(Ordering::SeqCst), 4);
    assert_eq!(pending(&env, chat).await, 0, "the six rows are done");
    assert_eq!(
        outcome,
        HandlerOutcome::Retry,
        "9 s are left after the last row: the store waits for the next delivery"
    );
    assert!(storage.calls("store").is_empty());
}

#[tokio::test]
async fn a_call_cut_by_the_provider_delete_timeout_is_a_failed_attempt() {
    let env = WorkerEnv::new().await;
    let chat = deleted_chat(&env, 1).await;
    let clock = ManualClock::new();
    // the provider never answers
    let storage = ClockedStorage::new(&clock, secs(3600));
    let service = Arc::new(env.chat_cleanup_service_with(
        Arc::clone(&storage) as Arc<dyn StorageClient>,
        Arc::clone(&clock) as Arc<dyn WorkerClock>,
    ));
    let delivery = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.handle(USER_TENANT, chat, 0).await })
    };

    let at = clock.advance_when_pending(2).await;
    let outcome = tokio::time::timeout(secs(10), delivery)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(at, secs(10), "PROVIDER_DELETE_TIMEOUT");
    assert_eq!(outcome, HandlerOutcome::Retry);
    let id = env.att_ids_of(chat).await[0];
    let snap = env.att(id).await;
    assert_eq!(
        snap.cleanup_attempts, 1,
        "a timeout of the call itself counts"
    );
    assert_eq!(snap.last_cleanup_error.as_deref(), Some("timeout"));
    assert_eq!(env.metrics.count(Metric::CleanupRetry(File)), 1);
}

#[tokio::test]
async fn the_call_bound_is_lowered_to_the_time_left_and_a_call_cut_by_the_deadline_is_deferred() {
    let env = WorkerEnv::new().await;
    let chat = deleted_chat(&env, 1).await;
    let clock = ManualClock::new();
    let storage = ClockedStorage::new(&clock, secs(3600));
    // a delivery budget of 4 s leaves 4 s of the 10 s PROVIDER_DELETE_TIMEOUT
    let service = Arc::new(env.chat_cleanup_service_budget(
        Arc::clone(&storage) as Arc<dyn StorageClient>,
        Arc::clone(&clock) as Arc<dyn WorkerClock>,
        secs(4),
    ));
    let delivery = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.handle(USER_TENANT, chat, 0).await })
    };

    let at = clock.advance_when_pending(2).await;
    let outcome = tokio::time::timeout(secs(10), delivery)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        at,
        secs(4),
        "the call is cut by the delivery deadline, not by the 10 s timeout"
    );
    assert_eq!(outcome, HandlerOutcome::Retry);
    let snap = env.att(env.att_ids_of(chat).await[0]).await;
    assert_eq!(
        snap.cleanup_attempts, 0,
        "a deferred call is not an attempt"
    );
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(env.metrics.count(Metric::CleanupRetry(File)), 0);
}
