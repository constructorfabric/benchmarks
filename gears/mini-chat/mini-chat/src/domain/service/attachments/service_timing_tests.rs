#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::field_reassign_with_default,
    clippy::needless_pass_by_value,
    clippy::type_complexity
)]
//! Slice 3: the time-bound parts of the upload
//! (`cpt-cf-mini-chat-algo-attachments-index-document`,
//! `-ensure-vector-store`, `-background-indexing`, the "External-call timeouts"
//! table of the FEATURE): the 25 s request deadline, the polling waits, the
//! hung provider calls, the vector store loser polls, and the background
//! continuation with its heartbeat, its 10 minute window and its stop rules.
//!
//! Paused tokio time over the in-memory attachment store: a real `SQLite` pool
//! cannot run under a paused clock, and the waits are the point here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use time::OffsetDateTime;
use tokio::time::Instant;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::model::{AttachmentDetail, AttachmentKind, AttachmentStatus, StoreRow};
use super::payload::CleanupEvent;
use super::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundResult, BackgroundRuntime, StorageClient,
    StorageDirectory, UploadResult,
};
use super::service::{
    AttachmentSettings, AttachmentsDeps, AttachmentsService, BACKGROUND_INDEXING_TASK,
};
use super::test_support::{
    FakeChats, FakeRepo, RecordingAttachmentMetrics, TestRuntime, VecSource, t0,
};
use crate::attachment_test_env::{att_catalog, att_directory, png_bytes, script_happy_on};
use crate::config::{MiniChatConfig, RagConfig};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::quota::service::Clock;
use crate::domain::service::quota::test_support::{FakePolicy, design_limits, snapshot};
use crate::infra::storage::oagw_storage::OagwStorageClient;
use crate::test_doubles::{
    FakeOagw, FakePdp, Fault, PdpReply, ProxyReply, RecordedProxy, S2S_SUBJECT, USER, USER_TENANT,
    user_ctx,
};

const PDF: &str = "application/pdf";
const FILES: &str = "/api.openai.test/v1/files";
const CREATE: &str = "/api.openai.test/v1/vector_stores";
const ADD: &str = "/api.openai.test/v1/vector_stores/*/files";
const STATUS: &str = "/api.openai.test/v1/vector_stores/*/files/*";
const ANTHROPIC_FILES: &str = "/api.anthropic.test/v1/files";

struct Svc {
    service: Arc<AttachmentsService>,
    repo: Arc<FakeRepo>,
    oagw: Arc<FakeOagw>,
    metrics: Arc<RecordingAttachmentMetrics>,
    runtime: Arc<TestRuntime>,
    start: Instant,
    base: OffsetDateTime,
    chat: Uuid,
}

fn svc_with(model: &str, rag: RagConfig, runtime: Arc<TestRuntime>) -> Svc {
    let chat = Uuid::from_u128(0xC000_0001);
    let repo = FakeRepo::new();
    let oagw = FakeOagw::new();
    script_happy_on(&oagw);
    let metrics = RecordingAttachmentMetrics::new();
    let start = Instant::now();
    let base = t0() + time::Duration::hours(1);
    let clock: Clock = Arc::new(move || base + (Instant::now() - start));
    let mut config = MiniChatConfig::default();
    config.rag = rag;
    let policy = FakePolicy::serving(snapshot(7, att_catalog()), design_limits(7));
    let service = Arc::new(AttachmentsService::new(AttachmentsDeps {
        authz: Arc::new(AuthzService::new(FakePdp::new(PdpReply::AllowScoped))),
        chats: FakeChats::with(vec![FakeChats::chat(chat, model)]),
        policy: policy as Arc<dyn PolicyCatalog>,
        directory: att_directory() as Arc<dyn StorageDirectory>,
        repo: Arc::clone(&repo) as Arc<dyn AttachmentRepository>,
        storage: Arc::new(OagwStorageClient::new(oagw.clone())) as Arc<dyn StorageClient>,
        metrics: Arc::clone(&metrics) as Arc<dyn AttachmentMetrics>,
        runtime: Arc::clone(&runtime) as Arc<dyn BackgroundRuntime>,
        settings: AttachmentSettings::from_config(&config),
        clock,
    }));
    Svc {
        service,
        repo,
        oagw,
        metrics,
        runtime,
        start,
        base,
        chat,
    }
}

fn svc() -> Svc {
    svc_with("m-oa", RagConfig::default(), TestRuntime::bound())
}

impl Svc {
    fn elapsed(&self) -> Duration {
        Instant::now() - self.start
    }

    fn now(&self) -> OffsetDateTime {
        self.base + self.elapsed()
    }

    async fn upload_as(
        &self,
        ctx: &SecurityContext,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<AttachmentDetail, DomainError> {
        let (mut source, _) = VecSource::file(Some(name), content_type, bytes, 1024);
        self.service.upload(ctx, self.chat, &mut source).await
    }

    async fn upload(
        &self,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<AttachmentDetail, DomainError> {
        self.upload_as(&user_ctx(), name, content_type, bytes).await
    }

    async fn pdf(&self) -> Result<AttachmentDetail, DomainError> {
        self.upload("a.pdf", PDF, b"%PDF-1.7").await
    }

    fn calls(&self, method: &str, suffix: &str) -> Vec<RecordedProxy> {
        self.oagw
            .proxied()
            .into_iter()
            .filter(|c| c.method == method && c.uri.split('?').next().unwrap().ends_with(suffix))
            .collect()
    }

    fn deletes(&self) -> Vec<RecordedProxy> {
        self.oagw
            .proxied()
            .into_iter()
            .filter(|c| c.method == "DELETE")
            .collect()
    }

    /// Scripts the add-file answer and makes every status read answer `status`.
    fn index_status(&self, add: serde_json::Value, status: serde_json::Value) {
        self.oagw.on_path_json("POST", ADD, 200, &add);
        self.oagw.on_path_json("GET", STATUS, 200, &status);
    }

    /// Records the virtual time of every status read; the answer comes from
    /// `answer(read number, elapsed)`.
    fn status_reads(
        &self,
        answer: impl Fn(usize, Duration) -> ProxyReply + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<(Duration, uuid::Uuid)>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let start = self.start;
        self.oagw.on_path("GET", STATUS, move |call| {
            let at = Instant::now() - start;
            let mut log = log.lock().unwrap();
            let n = log.len();
            log.push((at, call.subject));
            drop(log);
            answer(n, at)
        });
        seen
    }
}

fn in_progress() -> ProxyReply {
    ProxyReply::json(200, &json!({ "status": "in_progress" }))
}

fn completed() -> ProxyReply {
    ProxyReply::json(200, &json!({ "status": "completed" }))
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn close(actual: Duration, expected: Duration) -> bool {
    actual.abs_diff(expected) <= Duration::from_millis(5)
}

/// Lets spawned tasks run for `virtual_secs` of paused time.
async fn pass(virtual_secs: u64) {
    tokio::time::sleep(secs(virtual_secs)).await;
}

// ================================================================================================
// The request deadline
// ================================================================================================

#[tokio::test(start_paused = true)]
async fn a_document_still_indexing_at_the_deadline_is_uploaded_within_the_gateway_timeout() {
    let svc = svc();
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    let reads = svc.status_reads(|_, _| in_progress());

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Uploaded);
    let elapsed = svc.elapsed();
    assert!(elapsed >= secs(25) && elapsed < secs(30), "{elapsed:?}");
    let row = svc.repo.only_row();
    assert_eq!(row.status, AttachmentStatus::Uploaded);
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert!(!reads.lock().unwrap().is_empty());
    assert_eq!(
        svc.runtime.spawned(),
        vec![BACKGROUND_INDEXING_TASK],
        "the continuation is spawned"
    );
    assert_eq!(
        svc.metrics.uploads(),
        vec![(AttachmentKind::Document, UploadResult::Ok)]
    );
    assert_eq!(svc.metrics.pending_balance(), 0);
    svc.runtime.stop();
}

#[tokio::test(start_paused = true)]
async fn the_status_waits_start_at_250_ms_double_and_stop_growing_at_2_s() {
    let svc = svc();
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    let reads = svc.status_reads(|_, _| in_progress());

    svc.pdf().await.unwrap();

    let offsets: Vec<Duration> = reads.lock().unwrap().iter().map(|(at, _)| *at).collect();
    let gaps: Vec<Duration> = std::iter::once(offsets[0])
        .chain(offsets.windows(2).map(|w| w[1].checked_sub(w[0]).unwrap()))
        .collect();
    let expected = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        secs(1),
        secs(2),
        secs(2),
        secs(2),
    ];
    for (i, want) in expected.iter().enumerate() {
        assert!(
            close(gaps[i], *want),
            "gap {i}: {:?} vs {want:?} in {gaps:?}",
            gaps[i]
        );
    }
    assert!(
        offsets.iter().all(|at| *at < secs(25)),
        "no read starts past the deadline: {offsets:?}"
    );
    svc.runtime.stop();
}

#[tokio::test(start_paused = true)]
async fn the_status_reads_of_the_request_use_the_request_context() {
    let svc = svc();
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    let reads = svc.status_reads(|n, _| if n < 2 { in_progress() } else { completed() });

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert!(
        reads
            .lock()
            .unwrap()
            .iter()
            .all(|(_, subject)| *subject == USER)
    );
    assert_eq!(
        svc.runtime.spawned(),
        Vec::<&str>::new(),
        "no background task for a ready document"
    );
}

#[tokio::test(start_paused = true)]
async fn the_deadline_counts_from_the_upload_start_not_from_the_indexing_start() {
    let svc = svc();
    svc.oagw.on_path("POST", FILES, |_| {
        ProxyReply::json(200, &json!({ "id": "file-1" })).after(secs(20))
    });
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    svc.status_reads(|_, _| in_progress());

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Uploaded);
    let elapsed = svc.elapsed();
    assert!(
        elapsed >= secs(25) && elapsed < secs(26),
        "20 s of upload leave 5 s to wait: {elapsed:?}"
    );
    svc.runtime.stop();
}

#[tokio::test(start_paused = true)]
async fn a_slow_upload_with_quick_indexing_is_ready_after_the_upload_and_one_wait() {
    let svc = svc();
    svc.oagw.on_path("POST", FILES, |_| {
        ProxyReply::json(200, &json!({ "id": "file-1" })).after(secs(10))
    });
    svc.index_status(
        json!({ "status": "in_progress" }),
        json!({ "status": "completed" }),
    );

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert!(
        close(svc.elapsed(), Duration::from_millis(10_250)),
        "{:?}",
        svc.elapsed()
    );
}

// ================================================================================================
// Hung provider calls are cut by the deadline
// ================================================================================================

#[tokio::test(start_paused = true)]
async fn a_hung_file_upload_is_cut_at_the_deadline_and_fails_the_row_without_a_file() {
    let svc = svc();
    svc.oagw.on_path("POST", FILES, |_| ProxyReply::Pending);

    let result = svc.pdf().await;

    assert!(
        matches!(result, Err(DomainError::StorageUnavailable)),
        "{result:?}"
    );
    assert!(close(svc.elapsed(), secs(25)), "{:?}", svc.elapsed());
    let row = svc.repo.only_row();
    assert_eq!(row.status, AttachmentStatus::Failed);
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(row.provider_file_id, None);
    assert!(svc.repo.messages().is_empty());
    assert_eq!(
        svc.metrics.uploads(),
        vec![(AttachmentKind::Document, UploadResult::Failed)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_hung_store_creation_is_cut_at_the_deadline_and_the_file_is_cleaned_up() {
    let svc = svc();
    svc.oagw.on_path("POST", CREATE, |_| ProxyReply::Pending);

    let result = svc.pdf().await;

    assert!(
        matches!(result, Err(DomainError::StorageUnavailable)),
        "{result:?}"
    );
    assert!(close(svc.elapsed(), secs(25)), "{:?}", svc.elapsed());
    let row = svc.repo.only_row();
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = svc.repo.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].event, CleanupEvent::AttachmentUploadFailed);
    assert_eq!(messages[0].provider_file_id.as_deref(), Some("file-1"));
    assert!(svc.repo.stores().is_empty(), "the placeholder is deleted");
}

#[tokio::test(start_paused = true)]
async fn a_hung_add_file_is_cut_at_the_deadline_and_fails_the_indexing() {
    let svc = svc();
    svc.oagw.on_path("POST", ADD, |_| ProxyReply::Pending);

    let result = svc.pdf().await;

    assert!(
        matches!(result, Err(DomainError::StorageUnavailable)),
        "{result:?}"
    );
    assert!(close(svc.elapsed(), secs(25)));
    let row = svc.repo.only_row();
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    let messages = svc.repo.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].event, CleanupEvent::AttachmentIndexingFailed);
    assert!(svc.deletes().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_hung_status_read_ends_the_wait_with_uploaded_instead_of_failing() {
    let svc = svc();
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    svc.oagw.on_path("GET", STATUS, |_| ProxyReply::Pending);

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Uploaded);
    assert!(close(svc.elapsed(), secs(25)), "{:?}", svc.elapsed());
    assert_eq!(svc.repo.only_row().status, AttachmentStatus::Uploaded);
    svc.runtime.stop();
}

#[tokio::test(start_paused = true)]
async fn a_hung_secondary_copy_is_cut_and_never_fails_the_upload() {
    let svc = svc_with("m-an", RagConfig::default(), TestRuntime::bound());
    svc.oagw
        .on_path("POST", ANTHROPIC_FILES, |_| ProxyReply::Pending);

    let detail = svc
        .upload("a.png", "image/png", &png_bytes(16, 16))
        .await
        .unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert!(close(svc.elapsed(), secs(25)), "{:?}", svc.elapsed());
    assert_eq!(svc.repo.only_row().secondary_status, "failed");
}

// ================================================================================================
// The vector store loser
// ================================================================================================

fn placeholder(svc: &Svc, age: Duration) -> StoreRow {
    StoreRow {
        id: Uuid::new_v4(),
        tenant_id: USER_TENANT,
        chat_id: svc.chat,
        vector_store_id: None,
        provider: "openai".to_owned(),
        created_at: svc.now() - age,
    }
}

#[tokio::test(start_paused = true)]
async fn a_loser_polls_five_times_with_doubling_waits_and_then_fails_the_row() {
    let svc = svc();
    let waiting = placeholder(&svc, secs(1));
    svc.repo.put_store(waiting);

    let result = svc.pdf().await;

    assert!(
        matches!(result, Err(DomainError::StorageUnavailable)),
        "{result:?}"
    );
    assert!(
        close(
            svc.elapsed(),
            Duration::from_millis(250 + 500 + 1000 + 2000 + 4000)
        ),
        "waits of 250 ms, 500 ms, 1 s, 2 s and 4 s: {:?}",
        svc.elapsed()
    );
    assert_eq!(
        svc.repo.calls("vector_store"),
        7,
        "the provider_mismatch pre-check, the first read of the resolution and five polls"
    );
    assert!(
        svc.calls("POST", "/v1/vector_stores").is_empty(),
        "the loser never calls the provider"
    );
    let row = svc.repo.only_row();
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    assert_eq!(svc.repo.messages().len(), 1);
    assert_eq!(
        svc.repo.stores().len(),
        1,
        "the winner's placeholder is not touched"
    );
}

#[tokio::test(start_paused = true)]
async fn the_polls_never_run_past_the_deadline() {
    let svc = svc();
    svc.oagw.on_path("POST", FILES, |_| {
        ProxyReply::json(200, &json!({ "id": "file-1" })).after(secs(22))
    });
    svc.repo.put_store(placeholder(&svc, secs(1)));

    let result = svc.pdf().await;

    assert!(
        matches!(result, Err(DomainError::StorageUnavailable)),
        "{result:?}"
    );
    let elapsed = svc.elapsed();
    assert!(
        elapsed >= secs(22) && elapsed <= secs(25) + Duration::from_millis(5),
        "{elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_loser_that_sees_the_store_bound_proceeds_with_it() {
    let svc = svc();
    let waiting = placeholder(&svc, secs(1));
    let row_id = waiting.id;
    svc.repo.put_store(waiting);
    let repo = Arc::clone(&svc.repo);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            repo.bind_store(USER_TENANT, row_id, "vs_winner")
                .await
                .unwrap()
        );
    });

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert!(svc.calls("POST", "/v1/vector_stores").is_empty());
    assert_eq!(svc.calls("POST", "/vector_stores/vs_winner/files").len(), 1);
    assert!(
        close(svc.elapsed(), Duration::from_millis(750)),
        "bound at 600 ms, seen by the poll at 750 ms: {:?}",
        svc.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn a_placeholder_older_than_120_seconds_is_reclaimed() {
    let svc = svc();
    svc.repo.put_store(placeholder(&svc, secs(121)));

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(svc.calls("POST", "/v1/vector_stores").len(), 1);
    let stores = svc.repo.stores();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs_1"));
    assert!(svc.elapsed() < secs(1), "no polling: {:?}", svc.elapsed());
}

#[tokio::test(start_paused = true)]
async fn a_placeholder_of_exactly_120_seconds_is_still_waited_for() {
    let svc = svc();
    svc.repo.put_store(placeholder(&svc, secs(119)));

    let result = svc.pdf().await;

    assert!(
        matches!(result, Err(DomainError::StorageUnavailable)),
        "{result:?}"
    );
    assert!(svc.calls("POST", "/v1/vector_stores").is_empty());
}

// ================================================================================================
// The background continuation
// ================================================================================================

/// An upload that returned `uploaded`, with the status reads answered by `answer`.
async fn uploaded_with(
    svc: &Svc,
    answer: impl Fn(usize, Duration) -> ProxyReply + Send + Sync + 'static,
) -> (Uuid, Arc<Mutex<Vec<(Duration, Uuid)>>>) {
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    let reads = svc.status_reads(answer);
    let detail = svc.pdf().await.unwrap();
    assert_eq!(detail.status, AttachmentStatus::Uploaded, "{detail:?}");
    (detail.id, reads)
}

fn after_secs(limit: u64) -> impl Fn(usize, Duration) -> ProxyReply + Send + Sync + 'static {
    move |_, at| {
        if at >= secs(limit) {
            completed()
        } else {
            in_progress()
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_background_task_makes_the_row_ready_when_the_index_completes() {
    let svc = svc();
    let (id, reads) = uploaded_with(&svc, after_secs(40)).await;

    pass(30).await;

    let row = svc.repo.row(id);
    assert_eq!(row.status, AttachmentStatus::Ready);
    assert_eq!(svc.metrics.background(), vec![BackgroundResult::Ready]);
    assert!(
        reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(at, _)| *at > secs(26))
            .all(|(_, subject)| *subject == S2S_SUBJECT),
        "the background task uses the S2S context"
    );
    assert!(svc.repo.messages().is_empty());
    svc.runtime.join().await;
}

#[tokio::test(start_paused = true)]
async fn the_background_task_refreshes_updated_at_once_per_20_second_round() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, after_secs(500)).await;
    let spawned_at = svc.elapsed();

    pass(60).await;

    let beats: Vec<Duration> = svc
        .repo
        .state
        .lock()
        .unwrap()
        .heartbeats
        .iter()
        .filter(|(beat_id, _)| *beat_id == id)
        .map(|(_, at)| (*at - svc.base).unsigned_abs())
        .collect();
    assert!(beats.len() >= 3, "{beats:?}");
    for pair in beats.windows(2) {
        assert!(
            close(pair[1].checked_sub(pair[0]).unwrap(), secs(20)),
            "20 s apart: {beats:?}"
        );
    }
    assert!(
        beats[0] >= spawned_at && beats[0] <= spawned_at + secs(1),
        "the first round starts with a heartbeat"
    );
    assert_eq!(
        (svc.repo.row(id).updated_at - svc.base).unsigned_abs(),
        *beats.last().unwrap(),
        "updated_at advances with every heartbeat"
    );
    svc.runtime.stop();
    svc.runtime.join().await;
}

#[tokio::test(start_paused = true)]
async fn the_background_polling_waits_250_ms_doubling_up_to_5_s() {
    let svc = svc();
    let (_, reads) = uploaded_with(&svc, |_, _| in_progress()).await;
    let from = reads.lock().unwrap().len();

    pass(19).await;

    let all = reads.lock().unwrap().clone();
    let bg: Vec<Duration> = all[from..].iter().map(|(at, _)| *at).collect();
    let gaps: Vec<Duration> = bg
        .windows(2)
        .map(|w| w[1].checked_sub(w[0]).unwrap())
        .collect();
    assert!(gaps.len() >= 5, "{bg:?}");
    for (gap, want) in gaps.iter().zip([500_u64, 1000, 2000, 4000, 5000, 5000]) {
        assert!(close(*gap, Duration::from_millis(want)), "{gaps:?}");
    }
    svc.runtime.stop();
    svc.runtime.join().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_status_in_the_background_fails_the_row_with_one_durable_cleanup_message() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, |_, at| {
        if at >= secs(30) {
            ProxyReply::json(200, &json!({ "status": "failed" }))
        } else {
            in_progress()
        }
    })
    .await;

    pass(20).await;

    let row = svc.repo.row(id);
    assert_eq!(row.status, AttachmentStatus::Failed);
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = svc.repo.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].event, CleanupEvent::AttachmentIndexingFailed);
    assert_eq!(messages[0].provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(svc.metrics.background(), vec![BackgroundResult::Failed]);
    assert!(svc.deletes().is_empty(), "there is no inline delete");
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_or_unknown_status_in_the_background_also_fails_the_row() {
    for status in ["cancelled", "banana"] {
        let svc = svc();
        let (id, _) = uploaded_with(&svc, move |_, at| {
            if at >= secs(30) {
                ProxyReply::json(200, &json!({ "status": status }))
            } else {
                in_progress()
            }
        })
        .await;

        pass(20).await;

        assert_eq!(
            svc.repo.row(id).error_code.as_deref(),
            Some("indexing_failed"),
            "{status}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_non_transient_read_error_in_the_background_fails_the_row_but_transient_ones_do_not() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, |n, at| {
        if at < secs(30) {
            in_progress()
        } else if n % 2 == 0 {
            ProxyReply::json(503, &json!({}))
        } else {
            completed()
        }
    })
    .await;
    pass(30).await;
    assert_eq!(
        svc.repo.row(id).status,
        AttachmentStatus::Ready,
        "5xx keeps polling"
    );

    let svc = svc_with("m-oa", RagConfig::default(), TestRuntime::bound());
    let (id, _) = uploaded_with(&svc, |_, at| {
        if at < secs(30) {
            in_progress()
        } else {
            ProxyReply::json(404, &json!({}))
        }
    })
    .await;
    pass(20).await;
    assert_eq!(
        svc.repo.row(id).error_code.as_deref(),
        Some("indexing_failed"),
        "404 is not transient"
    );
}

#[tokio::test(start_paused = true)]
async fn the_background_task_gives_up_after_ten_minutes_and_fails_the_row() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, |_, _| in_progress()).await;
    let spawned_at = svc.elapsed();

    pass(700).await;

    let row = svc.repo.row(id);
    assert_eq!(row.status, AttachmentStatus::Failed);
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(svc.repo.messages().len(), 1);
    assert_eq!(svc.metrics.background(), vec![BackgroundResult::Timeout]);
    let heartbeats = svc.repo.state.lock().unwrap().heartbeats.len();
    assert!(
        (29..=31).contains(&heartbeats),
        "one heartbeat per 20 s round for 10 minutes: {heartbeats}"
    );
    let last =
        (svc.repo.state.lock().unwrap().heartbeats.last().unwrap().1 - svc.base).unsigned_abs();
    assert!(
        last >= spawned_at + secs(570) && last <= spawned_at + secs(610),
        "{last:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_background_task_stops_without_changes_when_the_row_is_deleted() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, |_, _| in_progress()).await;
    pass(5).await;

    svc.repo.edit(id, |r| {
        r.deleted_at = Some(t0());
        r.cleanup_status = Some("pending".to_owned());
    });
    pass(60).await;

    let row = svc.repo.row(id);
    assert_eq!(
        row.status,
        AttachmentStatus::Uploaded,
        "never ready, never failed"
    );
    assert!(svc.metrics.background().is_empty(), "no metric");
    assert!(svc.repo.messages().is_empty());
    assert_eq!(svc.runtime.running(), 0, "the task ended");
}

#[tokio::test(start_paused = true)]
async fn the_background_task_stops_when_the_chat_cleanup_owns_the_row() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, |_, _| in_progress()).await;

    svc.repo
        .edit(id, |r| r.cleanup_status = Some("pending".to_owned()));
    pass(40).await;

    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Uploaded);
    assert!(svc.metrics.background().is_empty());
    assert_eq!(svc.runtime.running(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_background_task_stops_with_the_gear_and_leaves_the_row_for_the_reaper() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, |_, _| in_progress()).await;
    pass(5).await;

    svc.runtime.stop();
    svc.runtime.join().await;

    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Uploaded);
    assert!(svc.metrics.background().is_empty());
    assert!(svc.repo.messages().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_ready_write_of_the_background_task_is_retried_up_to_four_times_1_2_and_4_seconds_apart()
 {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, after_secs(40)).await;
    svc.repo.fail("set_ready", 3);
    let before = svc.repo.calls("set_ready");

    pass(60).await;

    assert_eq!(svc.repo.calls("set_ready") - before, 4);
    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Ready);
    assert_eq!(svc.metrics.background(), vec![BackgroundResult::Ready]);
}

#[tokio::test(start_paused = true)]
async fn the_four_ready_attempts_are_spaced_1_2_and_4_seconds() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, after_secs(40)).await;
    svc.repo.fail("set_ready", 3);
    let seen_at = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen_at);
    let repo = Arc::clone(&svc.repo);
    let start = svc.start;
    tokio::spawn(async move {
        let mut last = repo.calls("set_ready");
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now = repo.calls("set_ready");
            if now != last {
                log.lock().unwrap().push(Instant::now() - start);
                last = now;
            }
        }
    });

    pass(60).await;

    let at = seen_at.lock().unwrap().clone();
    assert_eq!(at.len(), 4, "{at:?}");
    for (gap, want) in at
        .windows(2)
        .map(|w| w[1].checked_sub(w[0]).unwrap())
        .zip([1_u64, 2, 4])
    {
        assert!(
            gap.abs_diff(secs(want)) <= Duration::from_millis(150),
            "{at:?}"
        );
    }
    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Ready);
}

#[tokio::test(start_paused = true)]
async fn when_every_ready_attempt_fails_the_row_stays_uploaded_for_the_reaper() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, after_secs(40)).await;
    svc.repo.fail("set_ready", 4);
    let before = svc.repo.calls("set_ready");

    pass(60).await;

    assert_eq!(svc.repo.calls("set_ready") - before, 4);
    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Uploaded);
    assert_eq!(
        svc.metrics.background(),
        vec![BackgroundResult::SetReadyFailed]
    );
    assert!(svc.repo.messages().is_empty(), "no failure transaction");
}

#[tokio::test(start_paused = true)]
async fn a_taken_row_at_the_ready_write_ends_the_task_silently() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, after_secs(40)).await;
    svc.repo.edit(id, |r| r.provider_file_id = None);

    pass(60).await;

    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Uploaded);
    assert!(
        svc.metrics.background().is_empty(),
        "the deleting path owns the file"
    );
}

#[tokio::test(start_paused = true)]
async fn a_heartbeat_database_error_is_logged_and_the_round_continues() {
    let svc = svc();
    let (id, _) = uploaded_with(&svc, after_secs(50)).await;
    svc.repo.fail("heartbeat", 1);

    pass(60).await;

    assert_eq!(svc.repo.row(id).status, AttachmentStatus::Ready);
}

#[tokio::test(start_paused = true)]
async fn the_background_task_holds_no_upload_permit() {
    let svc = svc_with(
        "m-oa",
        RagConfig {
            max_concurrent_uploads: 1,
            ..RagConfig::default()
        },
        TestRuntime::bound(),
    );
    let (_, _) = uploaded_with(&svc, |_, _| in_progress()).await;

    let second = svc.upload("b.zip", "application/zip", b"x").await;

    assert!(
        matches!(second, Err(DomainError::UnsupportedContentType)),
        "the permit is free while the background task runs: {second:?}"
    );
    svc.runtime.stop();
}

#[tokio::test(start_paused = true)]
async fn before_the_gear_has_started_nothing_is_spawned_and_the_row_stays_uploaded() {
    let svc = svc_with("m-oa", RagConfig::default(), TestRuntime::unbound());
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    svc.status_reads(|_, _| in_progress());

    let detail = svc.pdf().await.unwrap();

    assert_eq!(detail.status, AttachmentStatus::Uploaded);
    assert!(svc.runtime.spawned().is_empty());
    assert_eq!(svc.repo.only_row().status, AttachmentStatus::Uploaded);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_request_leaves_the_row_uploaded_and_spawns_nothing() {
    let svc = svc();
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    svc.status_reads(|_, _| in_progress());

    let cancelled = tokio::time::timeout(secs(10), svc.pdf()).await;

    assert!(cancelled.is_err(), "the client gave up after 10 s");
    let row = svc.repo.only_row();
    assert_eq!(row.status, AttachmentStatus::Uploaded);
    assert!(svc.runtime.spawned().is_empty());
    assert!(svc.repo.messages().is_empty(), "the reaper finishes it");
    assert!(svc.deletes().is_empty());
}

// ================================================================================================
// Failures of the request itself
// ================================================================================================

#[tokio::test(start_paused = true)]
async fn the_ready_write_of_the_request_is_not_retried_and_fails_the_row_durably() {
    let svc = svc();
    svc.index_status(
        json!({ "status": "completed" }),
        json!({ "status": "completed" }),
    );
    svc.repo.fail("set_ready", 1);

    let result = svc.pdf().await;

    assert!(
        matches!(
            result,
            Err(DomainError::Database(_) | DomainError::Internal(_))
        ),
        "{result:?}"
    );
    assert_eq!(svc.repo.calls("set_ready"), 1);
    let row = svc.repo.only_row();
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(svc.repo.messages().len(), 1);
    assert_eq!(
        svc.repo.messages()[0].event,
        CleanupEvent::AttachmentUploadFailed
    );
}

#[tokio::test(start_paused = true)]
async fn transient_status_errors_in_the_request_keep_polling_until_the_deadline() {
    let svc = svc();
    svc.oagw
        .on_path_json("POST", ADD, 200, &json!({ "status": "in_progress" }));
    svc.oagw.on_path("GET", STATUS, |_| {
        ProxyReply::Error(Fault::DeadlineExceeded)
    });

    let detail = svc.pdf().await.unwrap();

    assert_eq!(
        detail.status,
        AttachmentStatus::Uploaded,
        "a gateway failure is transient"
    );
    assert!(svc.elapsed() >= secs(25));
    svc.runtime.stop();
}
