//! The configuration of the upload path: the fixed timing values (not configurable), the keys and
//! defaults the feature reads from `rag`, `thumbnail`, `upload_reaper`, `cleanup_worker` and
//! `outbox`, the compile-time heartbeat bound, and the production timer.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-configuration` (steps
//! `inst-cfg-rag`, `inst-cfg-thumbnail`, `inst-cfg-workers` and `inst-cfg-fixed`) and
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-background-indexing` (step
//! `inst-bg-heartbeat-bound`); definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::json;

use super::{
    BACKGROUND_LIMIT, BACKGROUND_ROUND, BACKGROUND_WAIT_INITIAL, BACKGROUND_WAIT_MAX,
    INDEX_WAIT_INITIAL, INDEX_WAIT_MAX, INDEXING_DEADLINE, LOSER_INITIAL_WAIT, LOSER_POLLS,
    MIN_STALE_AFTER_SECS, SET_READY_BACKOFF, STALE_PLACEHOLDER, Timer, TokioTimer,
};
use crate::config::MiniChatConfig;
use crate::domain::attachments::AttachmentSettings;

fn config() -> MiniChatConfig {
    MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" }
    }))
    .expect("valid config")
}

#[test]
fn the_fixed_values_are_the_ones_of_the_design() {
    // inst-cfg-fixed: 25 s indexing deadline, 250 ms doubling to 2 s in-request polling, 20 s
    // background rounds with 250 ms doubling to 5 s, 10-minute background limit, 4 set-ready
    // attempts 1, 2 and 4 s apart, 120 s stale placeholder, 5 loser polls
    assert_eq!(INDEXING_DEADLINE, Duration::from_secs(25));
    assert_eq!(INDEX_WAIT_INITIAL, Duration::from_millis(250));
    assert_eq!(INDEX_WAIT_MAX, Duration::from_secs(2));
    assert_eq!(BACKGROUND_ROUND, Duration::from_secs(20));
    assert_eq!(BACKGROUND_LIMIT, Duration::from_secs(600));
    assert_eq!(BACKGROUND_WAIT_INITIAL, Duration::from_millis(250));
    assert_eq!(BACKGROUND_WAIT_MAX, Duration::from_secs(5));
    assert_eq!(
        SET_READY_BACKOFF,
        [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4)
        ]
    );
    assert_eq!(STALE_PLACEHOLDER, Duration::from_secs(120));
    assert_eq!(LOSER_POLLS, 5);
    assert_eq!(LOSER_INITIAL_WAIT, Duration::from_millis(200));
}

#[test]
fn the_heartbeat_interval_is_at_most_half_the_minimum_stale_threshold() {
    // inst-bg-heartbeat-bound (also asserted at compile time)
    assert_eq!(MIN_STALE_AFTER_SECS, 60);
    assert!(BACKGROUND_ROUND.as_secs() * 2 <= MIN_STALE_AFTER_SECS);
}

#[test]
fn the_rag_keys_default_to_the_design_values() {
    // inst-cfg-rag
    let rag = config().rag;
    assert_eq!(rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(rag.max_documents_per_chat, 50);
    assert_eq!(rag.max_total_upload_mb_per_chat, 100);
    assert!(rag.allow_csv_upload);
    assert_eq!(rag.max_concurrent_uploads, 10);
}

#[test]
fn the_thumbnail_keys_default_to_the_design_values() {
    // inst-cfg-thumbnail
    let thumbnail = config().thumbnail;
    assert_eq!((thumbnail.width, thumbnail.height), (128, 128));
    assert_eq!(thumbnail.max_bytes, 131_072);
    assert_eq!(thumbnail.max_pixels, 100_000_000);
    assert_eq!(thumbnail.max_decode_bytes, 33_554_432);
}

#[test]
fn the_worker_and_queue_keys_default_to_the_design_values() {
    // inst-cfg-workers
    let config = config();
    assert!(config.upload_reaper.enabled);
    assert_eq!(config.upload_reaper.scan_interval_secs, 60);
    assert_eq!(config.upload_reaper.stale_after_secs, 300);
    assert_eq!(config.cleanup_worker.max_attempts, 5);
    assert_eq!(
        config.outbox.cleanup_queue_name,
        "mini-chat.attachment_cleanup"
    );
}

#[test]
fn the_attachment_settings_default_to_the_configuration_defaults() {
    let settings = AttachmentSettings::default();
    let config = config();
    assert_eq!(
        settings.rag.max_documents_per_chat,
        config.rag.max_documents_per_chat
    );
    assert_eq!(
        settings.rag.uploaded_file_max_size_kb,
        config.rag.uploaded_file_max_size_kb
    );
    assert_eq!(
        settings.thumbnail.max_decode_bytes,
        config.thumbnail.max_decode_bytes
    );
}

#[test]
fn this_feature_adds_no_configuration_key() {
    // inst-cfg-return: an unknown key in the sections it reads is still rejected
    for section in ["rag", "thumbnail"] {
        let result = MiniChatConfig::from_value(json!({
            "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
            section: { "attachment_bogus_key": 1 }
        }));
        assert!(
            result.is_err(),
            "{section}.attachment_bogus_key was accepted"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_production_timer_sleeps_for_the_requested_time() {
    let timer = TokioTimer;
    let before = timer.monotonic();
    timer.sleep(Duration::from_millis(250)).await;
    let after = timer.monotonic();
    assert!(
        after.checked_sub(before).unwrap() >= Duration::from_millis(250),
        "{before:?} -> {after:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_production_timer_is_monotonic() {
    let timer = TokioTimer;
    let mut previous = timer.monotonic();
    for _ in 0..5 {
        timer.sleep(Duration::from_millis(10)).await;
        let now = timer.monotonic();
        assert!(now >= previous);
        previous = now;
    }
    assert!(previous > Duration::ZERO);
}
