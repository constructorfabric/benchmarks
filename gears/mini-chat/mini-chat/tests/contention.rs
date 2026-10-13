//! Database contention: requests issued back to back on a file-backed `SQLite`
//! database (WAL, pooled connections) while the outbox workers commit the
//! previous turn's follow-up work never fail with a 500 `database is locked`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::http::{Method, StatusCode};
use mini_chat::domain::services::UploadTimings;
use mini_chat::testing::{SseCapture, TestApp, TestUser};
use mini_chat_sdk::TierLimits;
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;
const ITERATIONS: usize = 20;
/// Pool size of the file-backed database (the server default is 5).
const POOL: u32 = 4;
/// Chats streaming at the same time (more than the pool has connections).
const CONCURRENT_CHATS: u32 = 8;
const PDF: &str = "application/pdf";
const PDF_BYTES: &[u8] = b"%PDF-1.4\n% mini-chat contention test\n";

fn fast_timings() -> UploadTimings {
    UploadTimings {
        indexing_deadline: Duration::from_millis(400),
        poll_initial: Duration::from_millis(20),
        poll_max: Duration::from_millis(50),
        background_round: Duration::from_millis(150),
        background_limit: Duration::from_secs(20),
        background_poll_max: Duration::from_millis(40),
        set_ready_retry: Duration::from_millis(10),
        vector_store_poll_initial: Duration::from_millis(10),
        stale_placeholder_after: Duration::from_secs(120),
    }
}

fn unlimited() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 1 << 60,
        limit_monthly_credits_micro: 1 << 60,
    }
}

/// Per transaction label: how many attempts failed on contention and were
/// retried (the `tx` field of `with_tx_retry`'s WARN events).
fn retries() -> &'static Mutex<HashMap<String, u32>> {
    static RETRIES: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    RETRIES.get_or_init(|| {
        let layer = RetryCounter;
        tracing::subscriber::set_global_default(Registry::default().with(layer))
            .expect("first global subscriber of this test binary");
        Mutex::new(HashMap::new())
    })
}

fn retries_of(label: &str) -> u32 {
    retries().lock().unwrap().get(label).copied().unwrap_or(0)
}

struct RetryCounter;

#[derive(Default)]
struct TxField {
    tx: Option<String>,
    message: String,
}

impl Visit for TxField {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "tx" {
            self.tx = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for RetryCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut f = TxField::default();
        event.record(&mut f);
        if let Some(tx) = f.tx
            && f.message.contains("retrying")
        {
            *retries().lock().unwrap().entry(tx).or_insert(0) += 1;
        }
    }
}

async fn app() -> TestApp {
    TestApp::builder()
        .file_db(POOL)
        .upload_timings(fast_timings())
        .limits(unlimited(), unlimited())
        .build()
        .await
}

async fn create_chat(app: &TestApp) -> Uuid {
    let r = app.call(U, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn chat_updated_at(app: &TestApp, chat: Uuid) -> Value {
    let r = app
        .call(U, Method::GET, &format!("{CHATS}/{chat}"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    r.json["updated_at"].clone()
}

async fn send(app: &TestApp, chat: Uuid, text: &str) -> SseCapture {
    app.stream(
        U,
        &format!("{CHATS}/{chat}/messages:stream"),
        json!({"content": text}),
    )
    .await
}

/// The stream answered 200 and ended with `done`.
fn assert_done(c: &SseCapture, what: &str) {
    assert_eq!(c.status, StatusCode::OK, "{what}: {:?}", c.problem);
    let last: Option<&(String, Value)> = c.last();
    assert_eq!(
        last.map(|(name, _)| name.as_str()),
        Some("done"),
        "{what}: {:?}",
        c.names()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_stream_then_upload_never_500() {
    let app = app().await;
    for i in 0..ITERATIONS {
        let chat = create_chat(&app).await;
        assert_done(&send(&app, chat, "hello").await, &format!("stream {i}"));
        let up = app.upload(U, chat, "note.pdf", PDF, PDF_BYTES).await;
        assert_eq!(up.status, StatusCode::CREATED, "upload {i}: {}", up.json);
        assert_done(
            &send(&app, chat, "again").await,
            &format!("second stream {i}"),
        );
        let del = app
            .call(U, Method::DELETE, &format!("{CHATS}/{chat}"), None)
            .await;
        assert_eq!(
            del.status,
            StatusCode::NO_CONTENT,
            "delete {i}: {}",
            del.json
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sends_in_different_chats_never_500() {
    let app = app().await;
    let mut chats = Vec::new();
    for _ in 0..CONCURRENT_CHATS {
        chats.push(create_chat(&app).await);
    }
    for i in 0..ITERATIONS {
        let captures =
            futures::future::join_all(chats.iter().map(|&chat| send(&app, chat, "hello"))).await;
        for (n, c) in captures.iter().enumerate() {
            assert_done(c, &format!("iteration {i}, chat {n}"));
        }
    }
}

/// Upload, stream, delete the attachment, stream, upload again: the attachment
/// transactions take the write lock with their first statement, so they wait
/// (`busy_timeout`) for the outbox commits of the previous turn instead of
/// failing on a stale read snapshot and being retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_stream_then_attachment_delete_never_500_nor_retried() {
    retries();
    let app = app().await;
    for i in 0..ITERATIONS {
        let chat = create_chat(&app).await;
        let up = app.upload(U, chat, "note.pdf", PDF, PDF_BYTES).await;
        assert_eq!(up.status, StatusCode::CREATED, "upload {i}: {}", up.json);
        let id = up.json["id"].as_str().unwrap().to_owned();
        assert_done(&send(&app, chat, "hello").await, &format!("stream {i}"));
        let del = app
            .call(
                U,
                Method::DELETE,
                &format!("{CHATS}/{chat}/attachments/{id}"),
                None,
            )
            .await;
        assert_eq!(
            del.status,
            StatusCode::NO_CONTENT,
            "attachment delete {i}: {}",
            del.json
        );
        assert_done(
            &send(&app, chat, "again").await,
            &format!("second stream {i}"),
        );
        let before = chat_updated_at(&app, chat).await;
        let up = app.upload(U, chat, "other.pdf", PDF, PDF_BYTES).await;
        assert_eq!(up.status, StatusCode::CREATED, "upload 2 {i}: {}", up.json);
        assert_eq!(
            chat_updated_at(&app, chat).await,
            before,
            "the write lock taken by the upload leaves updated_at alone"
        );
    }
    assert_eq!(
        retries_of("attachment delete"),
        0,
        "attachment delete retried"
    );
    assert_eq!(
        retries_of("attachment insert"),
        0,
        "attachment insert retried"
    );
}
