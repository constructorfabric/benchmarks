#![allow(clippy::wildcard_imports)]
//! Streaming pipeline tests (batch 2): downgrade reasons, vision gating,
//! web citations, summary application, tool gating.

use uuid::Uuid;

use time::OffsetDateTime;
use super::*;
use crate::api::rest::dto::{CitationSource, QuotaDecisionKind, StreamMessageRequest, WebSearchConfig};
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;
use crate::test_support::StubPolicy;

/// Catalog with a non-vision standard model.
fn vision_gap_catalog() -> Vec<mini_chat_sdk::ModelCatalogEntry> {
    let mut premium = crate::test_support::catalog_entry("gpt-premium", mini_chat_sdk::ModelTier::Premium, true, true);
    premium.multimodal_capabilities = vec!["VISION_INPUT".to_owned()];
    let mut standard = crate::test_support::catalog_entry("gpt-standard", mini_chat_sdk::ModelTier::Standard, true, false);
    standard.multimodal_capabilities = vec![]; // no VISION_INPUT
    vec![premium, standard]
}

#[tokio::test]
async fn disabled_model_downgrades_with_model_disabled() {
    let mut policy = StubPolicy::default();
    // The chat's selected model exists but is disabled.
    policy.snapshot.model_catalog = {
        let mut cat = crate::test_support::StubPolicy::default_catalog();
        cat[0].enabled = false;
        cat
    };
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    // Seed a chat on the disabled model directly (create rejects disabled).
    let chat = create_chat(&bed.bed, None, Some("gpt-premium")).await;

    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let done = done(&events);
    assert_eq!(done.quota_decision, QuotaDecisionKind::Downgrade);
    assert_eq!(done.effective_model, "gpt-standard");
    assert_eq!(done.downgrade_reason.as_deref(), Some("model_disabled"));
}

#[tokio::test]
async fn vision_downgrade_rejected_before_provider() {
    let mut policy = StubPolicy::new(vision_gap_catalog(), mini_chat_sdk::KillSwitches::default());
    // Premium quota exhausted -> downgrade to standard (no vision).
    policy.premium.limit_daily_credits_micro = 1;
    let bed = stream_bed_with(std::sync::Arc::new(policy)).await;
    let chat = create_chat(&bed.bed, None, Some("gpt-premium")).await;

    // Upload an image and reference it.
    let png: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let plan = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "image/png")
        .await
        .expect("prepare");
    let att = bed
        .services
        .attachments
        .upload(
            &bed.bed.security,
            plan,
            crate::domain::attachment_service::UploadInput {
                filename: "pic.png".to_owned(),
                content_type: "image/png".to_owned(),
                bytes: bytes::Bytes::from(&png[..]),
            },
        )
        .await
        .expect("upload");

    let Err(err) = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "what is this?".to_owned(),
                request_id: None,
                attachment_ids: Some(vec![att.id]),
                web_search: None,
            },
        )
        .await
    else {
        panic!("expected VISION_NOT_SUPPORTED");
    };
    assert!(matches!(err, DomainError::VisionNotSupported), "{err:?}");
    assert_eq!(provider_calls(&bed.gateway), 0, "no provider call");
}

#[tokio::test]
async fn images_disabled_kill_switch() {
    let mut kill = mini_chat_sdk::KillSwitches::default();
    kill.disable_images = true;
    let bed = stream_bed_with(std::sync::Arc::new(StubPolicy::new(
        crate::test_support::StubPolicy::default_catalog(),
        kill,
    )))
    .await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Image upload rejected.
    let err = bed
        .services
        .attachments
        .prepare_upload(&bed.bed.security, chat.id, "image/png")
        .await;
    assert!(matches!(err, Err(DomainError::FeatureDisabled("images"))));
}

#[tokio::test]
async fn web_citations_mapped() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let resp_id = format!("resp-{}", Uuid::new_v4().as_simple());
    let response = serde_json::json!({
        "response": {
            "id": resp_id,
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": "According to the report, growth is strong.",
                    "annotations": [{
                        "type": "url_citation",
                        "url": "https://example.com/report",
                        "title": "Growth Report",
                        "start_index": 0,
                        "end_index": 23
                    }],
                }],
            }],
            "usage": {"input_tokens": 3, "output_tokens": 2},
        }
    });
    bed.gateway.stream_script.lock().expect("lock").replace(
        crate::test_support::CannedStream {
            events: vec![(
                "response.completed".to_owned(),
                serde_json::to_string(&response).expect("json"),
            )],
        },
    );
    let setup = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: Some(WebSearchConfig { enabled: true }),
            },
        )
        .await
        .expect("send");
    let events = match setup {
        StreamSetup::Live { receiver, .. } => drain(receiver).await,
        StreamSetup::Replay(events) => events,
    };
    let citations = events.iter().find_map(|e| match e {
        MiniChatSseEvent::Citations(c) => Some(c),
        _ => None,
    });
    let citations = citations.expect("citations");
    assert_eq!(citations.items.len(), 1);
    let c = &citations.items[0];
    assert_eq!(c.source, CitationSource::Web);
    assert_eq!(c.url.as_deref(), Some("https://example.com/report"));
    assert_eq!(c.title, "Growth Report");
    assert_eq!(c.snippet, "According to the report".to_owned());
    assert_eq!(c.span.as_ref().map(|s| (s.start, s.end)), Some((0, 23)));
}

#[tokio::test]
async fn summary_applied_flag_when_summary_exists() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Seed a summary directly.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let msg_id = Uuid::new_v4();
    bed.bed
        .ctx
        .repos
        .summaries
        .commit_cas(
            &conn,
            bed.bed.security.subject_tenant_id(),
            chat.id,
            None,
            "earlier summary",
            (OffsetDateTime::now_utc(), msg_id),
            25,
        )
        .await
        .expect("commit");
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let started = started(&events);
    let applied = started.thread_summary_applied.as_ref().expect("summary applied");
    assert_eq!(applied.token_estimate, 25);

    // The provider request includes the summary as a user message.
    let reqs = bed.gateway.requests.lock().expect("lock");
    let input = reqs[0]["input"].as_array().expect("input");
    let has_summary = input.iter().any(|m| {
        m["role"] == "user"
            && m["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("earlier summary"))
    });
    assert!(has_summary, "summary in context: {input:?}");
}

#[tokio::test]
async fn file_search_not_sent_without_documents() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    let reqs = bed.gateway.requests.lock().expect("lock");
    let tools = &reqs[0]["tools"];
    assert!(
        tools.is_null() || tools.as_array().is_none_or(|t| t.iter().all(|x| x["type"] != "file_search")),
        "no file_search tool: {tools:?}"
    );
}

#[tokio::test]
async fn daily_web_search_quota_rejects() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Drive the daily web search counter to the limit (75) in the repo.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let now = time::OffsetDateTime::now_utc();
    let day = crate::domain::services::periods::daily_start(now);
    bed.bed
        .ctx
        .repos
        .quota
        .add_reserve(
            &conn,
            bed.bed.security.subject_tenant_id(),
            bed.bed.security.subject_id(),
            crate::infra::storage::repo::quota::BucketKey {
                period_type: "daily",
                period_start: day,
                bucket: "total",
            },
            1,
        )
        .await
        .expect("seed row");
    bed.bed
        .ctx
        .repos
        .quota
        .settle(
            &conn,
            bed.bed.security.subject_tenant_id(),
            bed.bed.security.subject_id(),
            crate::infra::storage::repo::quota::BucketKey {
                period_type: "daily",
                period_start: day,
                bucket: "total",
            },
            1,
            1,
            0,
            0,
            75,
            0,
        )
        .await
        .expect("settle");
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: None,
                attachment_ids: None,
                web_search: Some(WebSearchConfig { enabled: true }),
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::QuotaExceeded("web_search"))));
    assert_eq!(provider_calls(&bed.gateway), 0);
}

#[tokio::test]
async fn incomplete_response_is_done() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let resp_id = format!("resp-{}", Uuid::new_v4().as_simple());
    bed.gateway.stream_script.lock().expect("lock").replace(
        crate::test_support::CannedStream {
            events: vec![(
                "response.incomplete".to_owned(),
                format!(
                    r#"{{"response": {{"id": "{resp_id}", "incomplete_details": {{"reason": "max_tokens"}}, "usage": {{"input_tokens": 8, "output_tokens": 4}}}}}}"#
                ),
            )],
        },
    );
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    assert!(done(&events).usage.input_tokens == 8);
    assert!(error(&events).is_none(), "incomplete is done, not error");
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let turn = bed
        .bed
        .ctx
        .repos
        .turns
        .latest(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id)
        .await
        .expect("turn")
        .expect("row");
    assert_eq!(turn.state, "completed");
    assert!(turn.error_code.is_none(), "incomplete reason not in error_code");
}
