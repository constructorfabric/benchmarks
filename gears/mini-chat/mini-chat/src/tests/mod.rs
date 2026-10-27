#![allow(clippy::wildcard_imports)]
//! Integration tests over in-memory SQLite with stub plugins and a stub
//! OAGW gateway.

pub mod attachments;
pub mod chat_crud;
pub mod messages;
pub mod quota;
pub mod streaming;
pub mod streaming2;
pub mod streaming3;
pub mod summary;
pub mod cleanup;
pub mod mutations2;
pub mod turns;

use toolkit_odata::ODataQuery;
use uuid::Uuid;

use crate::api::rest::dto::{MiniChatSseEvent, StreamMessageRequest};
use crate::domain::services::AppServices;
use crate::test_support::{TestBed, app_services, create_chat, drain};
use crate::test_gateway::StubGateway;

/// A bed wired for streaming (with the stub gateway).
pub struct StreamBed {
    /// The test bed.
    pub bed: TestBed,
    /// The stub gateway.
    pub gateway: std::sync::Arc<StubGateway>,
    /// The app services.
    pub services: AppServices,
    /// The outbox pipeline handle (kept alive for the test).
    pub outbox: toolkit_db::outbox::OutboxHandle,
}

/// Build a streaming-capable bed with the default catalog.
pub async fn stream_bed() -> StreamBed {
    stream_bed_with(std::sync::Arc::new(crate::test_support::StubPolicy::default())).await
}

/// Build a streaming-capable bed with a custom config (default catalog).
pub async fn stream_bed_cfg(cfg: crate::config::MiniChatConfig) -> StreamBed {
    let policy = std::sync::Arc::new(crate::test_support::StubPolicy::default());
    stream_bed_full(cfg, policy).await
}

/// Build a streaming-capable bed with a custom policy.
pub async fn stream_bed_with(policy: std::sync::Arc<crate::test_support::StubPolicy>) -> StreamBed {
    stream_bed_full(crate::config::MiniChatConfig::default(), policy).await
}

/// Full constructor.
pub async fn stream_bed_full(
    cfg: crate::config::MiniChatConfig,
    policy: std::sync::Arc<crate::test_support::StubPolicy>,
) -> StreamBed {
    // The test catalog has no gpt-4.1-mini; point the summary worker at the
    // standard model.
    let mut cfg = cfg;
    cfg.thread_summary_worker.summary_model_id = "gpt-standard".to_owned();
    let bed = crate::test_support::test_bed(cfg, policy).await;
    let gateway = crate::test_support::StubGateway::new();
    let services = app_services(&bed.ctx, gateway.clone());
    let outbox = crate::test_support::start_outbox(&bed.ctx, gateway.clone()).await;
    StreamBed {
        bed,
        gateway,
        services,
        outbox,
    }
}

/// Send a message and drain the SSE events.
pub async fn send_and_drain(
    svc: &AppServices,
    bed: &TestBed,
    chat_id: Uuid,
    content: &str,
    request_id: Option<Uuid>,
) -> Vec<MiniChatSseEvent> {
    let setup = svc
        .stream
        .send_message(
            &bed.security,
            chat_id,
            StreamMessageRequest {
                content: content.to_owned(),
                request_id,
                attachment_ids: None,
                web_search: None,
            },
        )
        .await
        .expect("send");
    match setup {
        crate::domain::stream_service::StreamSetup::Live { receiver, .. } => drain(receiver).await,
        crate::domain::stream_service::StreamSetup::Replay(events) => events,
    }
}

/// Extract the terminal event.
pub fn terminal(events: &[MiniChatSseEvent]) -> &MiniChatSseEvent {
    events.last().expect("terminal event")
}

/// The text accumulated from delta events.
pub fn text_of(events: &[MiniChatSseEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            MiniChatSseEvent::Delta(d) => Some(d.content.as_str()),
            _ => None,
        })
        .collect()
}

/// The done payload.
pub fn done(events: &[MiniChatSseEvent]) -> &crate::api::rest::dto::DoneData {
    events
        .iter()
        .find_map(|e| match e {
            MiniChatSseEvent::Done(d) => Some(d),
            _ => None,
        })
        .expect("done event")
}

/// The error payload.
pub fn error(events: &[MiniChatSseEvent]) -> Option<&crate::api::rest::dto::ErrorData> {
    events.iter().find_map(|e| match e {
        MiniChatSseEvent::Error(d) => Some(d),
        _ => None,
    })
}

/// The `stream_started` payload.
pub fn started(events: &[MiniChatSseEvent]) -> &crate::api::rest::dto::StreamStartedData {
    events
        .iter()
        .find_map(|e| match e {
            MiniChatSseEvent::StreamStarted(d) => Some(d),
            _ => None,
        })
        .expect("stream_started event")
}

/// Number of provider chat requests the stub gateway received.
pub fn provider_calls(gateway: &StubGateway) -> usize {
    gateway.requests.lock().expect("lock").len()
}

/// Unwrap an expected error without requiring Debug on the Ok type.
pub fn expect_err<T>(r: Result<T, crate::error::DomainError>) -> crate::error::DomainError {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

/// Wait until a condition holds (poll, 5 s timeout).
pub async fn wait_until<F: FnMut() -> bool>(mut cond: F, what: &str) {
    let start = std::time::Instant::now();
    while !cond() {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A simple `OData` query.
pub fn odata() -> ODataQuery {
    ODataQuery::new()
}
