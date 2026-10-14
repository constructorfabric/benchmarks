//! The per-chat vector store protocol over fake ports: reuse, placeholder insert, single-winner
//! create, compare-and-set, the create failure cleanup, the lost-CAS cleanup, the 5-poll loser
//! path (200 ms doubling), the 120 s stale placeholder reclaim and the provider mismatch. The
//! waits run on the injected virtual timer.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-vector-store-provisioning`;
//! state `cpt-cf-mini-chat-state-attachments-and-file-search-chat-vector-store`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-vector-store`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use uuid::Uuid;

use super::{VectorStoreDeps, VectorStoreFailure, VectorStoreRow, provision_vector_store};
use crate::domain::attachments::provider::ProviderFailure;
use crate::domain::quota::Clock;
use crate::test_support::attach_fakes::{
    MemoryVectorRepo, ProviderCall, RepoCall, ScriptedProvider, openai_target,
};
use crate::test_support::manual_time::ManualTime;
use crate::test_support::provider::Gate;
use crate::test_support::quota::default_now;
use crate::test_support::{TENANT_A, TENANT_B};

const MS: fn(u64) -> Duration = Duration::from_millis;

struct Harness {
    time: Arc<ManualTime>,
    repo: Arc<MemoryVectorRepo>,
    provider: Arc<ScriptedProvider>,
    chat_id: Uuid,
}

fn harness_on(time: Arc<ManualTime>) -> Harness {
    let repo = MemoryVectorRepo::new(&time);
    let provider = ScriptedProvider::new(&time);
    Harness {
        time,
        repo,
        provider,
        chat_id: Uuid::new_v4(),
    }
}

fn harness() -> Harness {
    harness_on(ManualTime::auto(default_now()))
}

impl Harness {
    async fn provision(&self) -> Result<String, VectorStoreFailure> {
        let target = openai_target();
        let deps = VectorStoreDeps {
            repo: &*self.repo,
            provider: &*self.provider,
            clock: &*self.time,
            timer: &*self.time,
            target: &target,
        };
        provision_vector_store(&deps, TENANT_A, self.chat_id).await
    }

    fn row(&self, vector_store_id: Option<&str>, provider: &str, age: Duration) -> VectorStoreRow {
        VectorStoreRow {
            id: Uuid::new_v4(),
            tenant_id: TENANT_A,
            chat_id: self.chat_id,
            vector_store_id: vector_store_id.map(str::to_owned),
            provider: provider.to_owned(),
            file_count: 0,
            created_at: self.time.now() - age,
        }
    }

    fn creates(&self) -> usize {
        self.provider
            .count_where(|call| matches!(call, ProviderCall::CreateVectorStore))
    }
}

fn mismatch(result: &Result<String, VectorStoreFailure>) -> bool {
    matches!(result, Err(VectorStoreFailure::ProviderMismatch))
}

fn failed(result: &Result<String, VectorStoreFailure>) -> bool {
    matches!(result, Err(VectorStoreFailure::VectorStoreFailed))
}

// ── fast path ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bound_store_is_reused_without_any_write_or_provider_call() {
    // inst-vs-fast-path, inst-vs-return-reuse
    let h = harness();
    h.repo
        .seed(h.row(Some("vs_existing"), "azure_openai", MS(0)));
    assert_eq!(h.provision().await.unwrap(), "vs_existing");
    assert_eq!(h.creates(), 0);
    assert!(h.provider.calls().is_empty());
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::InsertPlaceholder { .. })),
        0
    );
}

#[tokio::test]
async fn a_bound_store_of_another_backend_is_a_provider_mismatch() {
    // inst-vs-mismatch
    let h = harness();
    h.repo
        .seed(h.row(Some("vs_existing"), "some_other_backend", MS(0)));
    assert!(mismatch(&h.provision().await));
    assert_eq!(h.creates(), 0);
}

#[tokio::test]
async fn the_store_of_another_chat_is_never_reused() {
    // tenant isolation: access only by (tenant_id, chat_id)
    let h = harness();
    let mut other_chat = h.row(Some("vs_other_chat"), "azure_openai", MS(0));
    other_chat.chat_id = Uuid::new_v4();
    h.repo.seed(other_chat);
    let mut other_tenant = h.row(Some("vs_other_tenant"), "azure_openai", MS(0));
    other_tenant.tenant_id = TENANT_B;
    h.repo.seed(other_tenant);
    assert_eq!(h.provision().await.unwrap(), "vs_created");
    assert_eq!(h.creates(), 1);
}

// ── winner path ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_first_upload_inserts_a_placeholder_creates_the_store_and_binds_it() {
    // inst-vs-insert, inst-vs-create, inst-vs-cas, inst-vs-return-created
    let h = harness();
    assert_eq!(h.provision().await.unwrap(), "vs_created");
    let rows = h.repo.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].vector_store_id.as_deref(), Some("vs_created"));
    assert_eq!(rows[0].provider, "azure_openai");
    assert_eq!(rows[0].file_count, 0);
    assert_eq!((rows[0].tenant_id, rows[0].chat_id), (TENANT_A, h.chat_id));
    assert_eq!(h.creates(), 1);
}

#[tokio::test]
async fn the_placeholder_is_inserted_before_the_provider_create() {
    let h = harness();
    h.provision().await.unwrap();
    let repo_calls = h.repo.calls();
    assert!(matches!(repo_calls[0], RepoCall::Find { .. }));
    assert!(
        matches!(&repo_calls[1], RepoCall::InsertPlaceholder { provider } if provider == "azure_openai")
    );
    assert!(
        matches!(&repo_calls[2], RepoCall::SetVectorStoreId { vector_store_id } if vector_store_id == "vs_created")
    );
}

#[tokio::test]
async fn a_failed_create_deletes_the_placeholder_and_fails() {
    // inst-vs-create-failed
    let h = harness();
    h.provider.queue_create(Err(ProviderFailure::Other));
    assert!(failed(&h.provision().await));
    assert!(h.repo.rows().is_empty(), "the placeholder was left behind");
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::DeletePlaceholder)),
        1
    );
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::SetVectorStoreId { .. })),
        0
    );
}

#[tokio::test]
async fn a_failed_create_is_not_retried() {
    let h = harness();
    h.provider.queue_create(Err(ProviderFailure::Transient));
    assert!(failed(&h.provision().await));
    assert_eq!(h.creates(), 1);
}

#[tokio::test]
async fn a_lost_compare_and_set_deletes_the_new_store_and_uses_the_winner() {
    // inst-vs-cas-lost: the placeholder was reclaimed meanwhile; delete the new store best
    // effort and take the loser path
    let h = harness();
    let gate = Gate::new();
    h.provider.gate_create(&gate);
    let target = openai_target();
    let task = {
        let (repo, provider, time, chat_id) = (
            h.repo.clone(),
            h.provider.clone(),
            h.time.clone(),
            h.chat_id,
        );
        tokio::spawn(async move {
            let deps = VectorStoreDeps {
                repo: &*repo,
                provider: &*provider,
                clock: &*time,
                timer: &*time,
                target: &target,
            };
            provision_vector_store(&deps, TENANT_A, chat_id).await
        })
    };
    for _ in 0..500 {
        if h.creates() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(h.creates(), 1, "the create never started");
    // another request reclaimed the placeholder and bound its own store
    h.repo.clear();
    h.repo.seed(h.row(Some("vs_winner"), "azure_openai", MS(0)));
    gate.open();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("provisioning never ended")
        .unwrap();
    assert_eq!(result.unwrap(), "vs_winner");
    assert_eq!(
        h.provider.count_where(
            |call| matches!(call, ProviderCall::DeleteVectorStore(id) if id == "vs_created")
        ),
        1,
        "the orphaned provider store was not deleted"
    );
    assert_eq!(h.creates(), 1);
}

// ── loser path ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_fresh_placeholder_makes_the_request_a_loser_that_never_creates() {
    // inst-vs-in-progress, inst-vs-loser-poll, inst-vs-return-loser-failed: 5 polls, 200 ms
    // before the first and doubling (about 6.2 s), then vector_store_failed
    let h = harness();
    h.repo.seed(h.row(None, "azure_openai", MS(500)));
    assert!(failed(&h.provision().await));
    assert_eq!(h.creates(), 0);
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::InsertPlaceholder { .. })),
        0
    );
    assert_eq!(
        h.time.sleeps(),
        vec![MS(200), MS(400), MS(800), MS(1600), MS(3200)]
    );
    assert_eq!(h.time.elapsed(), MS(6200));
}

#[tokio::test]
async fn the_loser_returns_the_store_as_soon_as_it_appears() {
    // inst-vs-loser-read
    let time = ManualTime::manual(default_now());
    let h = Arc::new(harness_on(time.clone()));
    h.repo.seed(h.row(None, "azure_openai", MS(500)));
    let task = {
        let h = Arc::clone(&h);
        tokio::spawn(async move { h.provision().await })
    };
    time.wait_for_parked(1).await;
    time.advance(MS(200)); // poll 1: still a placeholder
    time.wait_for_parked(1).await;
    h.repo.bind("vs_winner");
    time.advance(MS(400)); // poll 2 sees the store
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("provisioning never ended")
        .unwrap();
    assert_eq!(result.unwrap(), "vs_winner");
    assert_eq!(h.creates(), 0);
    assert_eq!(time.sleeps(), vec![MS(200), MS(400)]);
}

#[tokio::test]
async fn the_loser_applies_the_provider_check_to_the_store_it_finds() {
    // inst-vs-loser-read: apply the provider check of step 2
    let time = ManualTime::manual(default_now());
    let h = Arc::new(harness_on(time.clone()));
    h.repo.seed(h.row(None, "some_other_backend", MS(500)));
    let task = {
        let h = Arc::clone(&h);
        tokio::spawn(async move { h.provision().await })
    };
    time.wait_for_parked(1).await;
    h.repo.bind("vs_other_backend");
    time.advance(MS(200));
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("provisioning never ended")
        .unwrap();
    assert!(mismatch(&result), "{result:?}");
}

#[tokio::test]
async fn losing_the_unique_insert_race_goes_to_the_loser_path_without_creating() {
    // inst-vs-lost, inst-vs-goto-loser-race: the loser never calls provider create
    let h = harness();
    h.repo.lose_next_insert(Some("vs_won"));
    assert_eq!(h.provision().await.unwrap(), "vs_won");
    assert_eq!(h.creates(), 0);
    assert_eq!(h.time.sleeps(), vec![MS(200)]);
}

#[tokio::test]
async fn losing_the_insert_race_to_a_placeholder_waits_for_it_and_fails_when_it_never_binds() {
    let h = harness();
    h.repo.lose_next_insert(None);
    assert!(failed(&h.provision().await));
    assert_eq!(h.creates(), 0);
    assert_eq!(h.time.sleeps().len(), 5);
}

// ── stale placeholder ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_placeholder_older_than_120_seconds_is_reclaimed_and_the_protocol_restarts_once() {
    // inst-vs-stale, inst-vs-reclaim, state placeholder -> absent
    let h = harness();
    h.repo
        .seed(h.row(None, "azure_openai", Duration::from_secs(121)));
    assert_eq!(h.provision().await.unwrap(), "vs_created");
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::DeletePlaceholder)),
        1
    );
    assert_eq!(h.creates(), 1);
    let rows = h.repo.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].vector_store_id.as_deref(), Some("vs_created"));
    assert!(
        h.time.sleeps().is_empty(),
        "a reclaim never takes the loser path"
    );
}

#[tokio::test]
async fn a_placeholder_of_exactly_120_seconds_is_not_stale() {
    // "more than 120 s old"
    let h = harness();
    h.repo
        .seed(h.row(None, "azure_openai", Duration::from_secs(120)));
    assert!(failed(&h.provision().await));
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::DeletePlaceholder)),
        0
    );
    assert_eq!(h.creates(), 0);
}

#[tokio::test]
async fn a_stale_placeholder_of_another_chat_is_left_alone() {
    let h = harness();
    let mut foreign = h.row(None, "azure_openai", Duration::from_secs(600));
    foreign.chat_id = Uuid::new_v4();
    h.repo.seed(foreign);
    assert_eq!(h.provision().await.unwrap(), "vs_created");
    assert_eq!(h.repo.rows().len(), 2);
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::DeletePlaceholder)),
        0
    );
}

#[tokio::test]
async fn the_age_is_measured_on_the_application_clock() {
    // a placeholder created in the future of the clock is never stale
    let h = harness();
    let mut row = h.row(None, "azure_openai", MS(0));
    row.created_at = default_now() + time::Duration::seconds(600);
    h.repo.seed(row);
    assert!(failed(&h.provision().await));
    assert_eq!(
        h.repo
            .count(|call| matches!(call, RepoCall::DeletePlaceholder)),
        0
    );
    let _ = OffsetDateTime::UNIX_EPOCH;
}
