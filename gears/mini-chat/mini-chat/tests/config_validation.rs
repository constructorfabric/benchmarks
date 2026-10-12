#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Gear configuration validation (DESIGN Appendix B.1, B.4, B.5, B.9, B.10).
//!
//! `TestEnvBuilder::gear_config` deep-merges a patch over the harness defaults and the
//! result is validated exactly like production: `try_build()` must fail for an invalid
//! configuration and succeed for the defaults and for every documented boundary value.

mod common;

use common::{CHATS_PATH, chat_id, expect_status, get};
use http::{Method, StatusCode};
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};

async fn build_with(patch: Value) -> Result<TestEnv, anyhow::Error> {
    TestEnvBuilder::new().gear_config(patch).try_build().await
}

async fn assert_rejected(patch: Value) {
    let outcome = build_with(patch.clone()).await;
    assert!(outcome.is_err(), "config must be rejected at init: {patch}");
}

async fn assert_accepted(patch: Value) {
    let outcome = build_with(patch.clone()).await;
    assert!(outcome.is_ok(), "config must be accepted: {patch}");
}

// ===========================================================================
// Defaults
// ===========================================================================

#[tokio::test]
async fn harness_default_config_builds() {
    let outcome = TestEnvBuilder::new().try_build().await;

    assert!(outcome.is_ok(), "the documented defaults must be valid");
}

#[tokio::test]
async fn empty_config_patch_builds() {
    assert_accepted(json!({})).await;
}

// ===========================================================================
// Unknown keys are rejected (top level and strict sections)
// ===========================================================================

#[tokio::test]
async fn unknown_top_level_key_fails_startup() {
    assert_rejected(json!({ "bogus_key": 1 })).await;
}

#[tokio::test]
async fn misspelled_top_level_key_fails_startup() {
    assert_rejected(json!({ "url_prefx": "/mini-chat" })).await;
}

#[tokio::test]
async fn unknown_key_in_streaming_section_fails_startup() {
    assert_rejected(json!({ "streaming": { "bogus_key": 1 } })).await;
}

#[tokio::test]
async fn unknown_key_in_quota_section_fails_startup() {
    assert_rejected(json!({ "quota": { "bogus_key": 1 } })).await;
}

#[tokio::test]
async fn unknown_key_in_provider_entry_fails_startup() {
    assert_rejected(json!({ "providers": { "openai": { "bogus_key": 1 } } })).await;
}

#[tokio::test]
async fn unknown_key_in_provider_tenant_override_fails_startup() {
    let tenant = uuid::Uuid::new_v4().to_string();
    assert_rejected(json!({
        "providers": { "openai": { "tenant_overrides": {
            tenant: { "host": "tenant.llm.test", "bogus_key": 1 }
        } } }
    }))
    .await;
}

#[tokio::test]
async fn unknown_key_in_every_other_strict_section_fails_startup() {
    let sections = [
        "estimation_budgets",
        "outbox",
        "context",
        "rag",
        "client_credentials",
        "metrics",
        "thumbnail",
        "knowledge_search",
    ];

    for section in sections {
        assert_rejected(json!({ section: { "bogus_key": 1 } })).await;
    }
}

#[tokio::test]
async fn removed_streaming_web_search_context_size_key_fails_startup() {
    assert_rejected(json!({ "streaming": { "web_search_context_size": "low" } })).await;
}

#[tokio::test]
async fn removed_provider_supports_file_search_filters_key_fails_startup() {
    assert_rejected(json!({
        "providers": { "openai": { "supports_file_search_filters": true } }
    }))
    .await;
}

#[tokio::test]
async fn worker_sections_accept_and_ignore_unknown_keys() {
    let sections = [
        "orphan_watchdog",
        "upload_reaper",
        "thread_summary_worker",
        "cleanup_worker",
    ];

    for section in sections {
        assert_accepted(json!({ section: { "bogus_key": 1 } })).await;
    }
}

// ===========================================================================
// Out-of-range values are rejected
// ===========================================================================

#[tokio::test]
async fn sse_ping_interval_below_5_seconds_fails_startup() {
    assert_rejected(json!({ "streaming": { "sse_ping_interval_seconds": 3 } })).await;
}

#[tokio::test]
async fn sse_ping_interval_above_60_seconds_fails_startup() {
    assert_rejected(json!({ "streaming": { "sse_ping_interval_seconds": 61 } })).await;
}

#[tokio::test]
async fn sse_channel_capacity_outside_16_to_64_fails_startup() {
    for capacity in [8, 15, 65, 128] {
        assert_rejected(json!({ "streaming": { "sse_channel_capacity": capacity } })).await;
    }
}

#[tokio::test]
async fn orphan_watchdog_timeout_below_90_seconds_fails_startup() {
    assert_rejected(json!({ "orphan_watchdog": { "timeout_secs": 30 } })).await;
}

#[tokio::test]
async fn orphan_watchdog_timeout_above_3600_seconds_fails_startup() {
    assert_rejected(json!({ "orphan_watchdog": { "timeout_secs": 3601 } })).await;
}

#[tokio::test]
async fn quota_overshoot_tolerance_factor_above_1_5_fails_startup() {
    assert_rejected(json!({ "quota": { "overshoot_tolerance_factor": 2.0 } })).await;
}

#[tokio::test]
async fn quota_overshoot_tolerance_factor_below_1_0_fails_startup() {
    assert_rejected(json!({ "quota": { "overshoot_tolerance_factor": 0.9 } })).await;
}

#[tokio::test]
async fn quota_warning_threshold_outside_1_to_99_fails_startup() {
    for pct in [0, 100] {
        assert_rejected(json!({ "quota": { "warning_threshold_pct": pct } })).await;
    }
}

#[tokio::test]
async fn outbox_num_partitions_that_is_not_a_power_of_two_fails_startup() {
    for partitions in [3, 6, 12] {
        assert_rejected(json!({ "outbox": { "num_partitions": partitions } })).await;
    }
}

#[tokio::test]
async fn outbox_num_partitions_outside_1_to_64_fails_startup() {
    for partitions in [0, 128] {
        assert_rejected(json!({ "outbox": { "num_partitions": partitions } })).await;
    }
}

#[tokio::test]
async fn outbox_queue_name_that_is_empty_fails_startup() {
    assert_rejected(json!({ "outbox": { "queue_name": "" } })).await;
}

#[tokio::test]
async fn context_recent_messages_limit_above_100_fails_startup() {
    assert_rejected(json!({ "context": { "recent_messages_limit": 101 } })).await;
}

#[tokio::test]
async fn minimal_generation_floor_of_zero_fails_startup() {
    assert_rejected(json!({ "estimation_budgets": { "minimal_generation_floor": 0 } })).await;
}

#[tokio::test]
async fn minimal_generation_floor_above_streaming_max_output_tokens_fails_startup() {
    assert_rejected(json!({
        "streaming": { "max_output_tokens": 100 },
        "estimation_budgets": { "minimal_generation_floor": 200 }
    }))
    .await;
}

// ===========================================================================
// Boundary values and valid overrides build
// ===========================================================================

#[tokio::test]
async fn documented_boundary_values_build() {
    let patches = vec![
        json!({ "streaming": { "sse_ping_interval_seconds": 5 } }),
        json!({ "streaming": { "sse_ping_interval_seconds": 60 } }),
        json!({ "streaming": { "sse_channel_capacity": 16 } }),
        json!({ "streaming": { "sse_channel_capacity": 64 } }),
        json!({ "orphan_watchdog": { "timeout_secs": 90 } }),
        json!({ "orphan_watchdog": { "timeout_secs": 3600 } }),
        json!({ "quota": { "overshoot_tolerance_factor": 1.0 } }),
        json!({ "quota": { "overshoot_tolerance_factor": 1.5 } }),
        json!({ "quota": { "warning_threshold_pct": 1 } }),
        json!({ "quota": { "warning_threshold_pct": 99 } }),
        json!({ "outbox": { "num_partitions": 1 } }),
        json!({ "outbox": { "num_partitions": 64 } }),
        json!({ "context": { "recent_messages_limit": 0 } }),
        json!({ "context": { "recent_messages_limit": 100 } }),
        json!({ "estimation_budgets": { "minimal_generation_floor": 1 } }),
    ];

    for patch in patches {
        assert_accepted(patch).await;
    }
}

// ===========================================================================
// Provider entries
// ===========================================================================

#[tokio::test]
async fn azure_storage_without_api_version_fails_startup() {
    assert_rejected(json!({ "providers": { "openai": { "storage_kind": "azure" } } })).await;
}

#[tokio::test]
async fn azure_storage_with_blank_api_version_fails_startup() {
    assert_rejected(json!({
        "providers": { "openai": { "storage_kind": "azure", "api_version": "  " } }
    }))
    .await;
}

#[tokio::test]
async fn azure_storage_with_an_api_version_containing_forbidden_characters_fails_startup() {
    assert_rejected(json!({
        "providers": { "openai": { "storage_kind": "azure", "api_version": "2025&x=1" } }
    }))
    .await;
}

#[tokio::test]
async fn azure_storage_with_api_version_builds() {
    assert_accepted(json!({
        "providers": { "openai": { "storage_kind": "azure", "api_version": "2025-03-01-preview" } }
    }))
    .await;
}

#[tokio::test]
async fn provider_rag_provider_naming_a_missing_entry_fails_startup() {
    assert_rejected(json!({ "providers": { "openai": { "rag_provider": "missing" } } })).await;
}

#[tokio::test]
async fn provider_with_empty_host_fails_startup() {
    assert_rejected(json!({ "providers": { "openai": { "host": "" } } })).await;
}

#[tokio::test]
async fn provider_host_with_path_characters_fails_startup() {
    assert_rejected(json!({ "providers": { "openai": { "host": "llm.test/evil?x=1" } } })).await;
}

#[tokio::test]
async fn provider_with_port_zero_fails_startup() {
    assert_rejected(json!({ "providers": { "openai": { "port": 0 } } })).await;
}

#[tokio::test]
async fn provider_without_storage_kind_fails_startup() {
    assert_rejected(json!({ "providers": { "other": {
        "kind": "openai_responses",
        "host": "other.llm.test"
    } } }))
    .await;
}

#[tokio::test]
async fn provider_with_unknown_kind_fails_startup() {
    assert_rejected(json!({ "providers": { "other": {
        "kind": "no_such_adapter",
        "host": "other.llm.test",
        "storage_kind": "openai"
    } } }))
    .await;
}

// ===========================================================================
// Identity and credentials
// ===========================================================================

#[tokio::test]
async fn empty_vendor_fails_startup() {
    assert_rejected(json!({ "vendor": "" })).await;
}

#[tokio::test]
async fn empty_client_id_fails_startup() {
    assert_rejected(json!({ "client_credentials": { "client_id": "" } })).await;
}

#[tokio::test]
async fn empty_client_secret_fails_startup() {
    assert_rejected(json!({ "client_credentials": { "client_secret": "" } })).await;
}

// ===========================================================================
// url_prefix moves every route
// ===========================================================================

#[tokio::test]
async fn url_prefix_config_serves_the_routes_under_the_configured_prefix() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "url_prefix": "/chat-api" }))
        .build()
        .await;
    let user = TestUser::new();

    let resp = get(&env, &user, "/chat-api/v1/chats").await;

    expect_status(&resp, StatusCode::OK);
    assert!(resp.json()["items"].is_array());
}

#[tokio::test]
async fn url_prefix_config_does_not_serve_the_default_prefix() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "url_prefix": "/chat-api" }))
        .build()
        .await;

    let resp = get(&env, &TestUser::new(), CHATS_PATH).await;

    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_chat_location_follows_the_configured_url_prefix() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "url_prefix": "/chat-api" }))
        .build()
        .await;
    let user = TestUser::new();

    let resp = env
        .request(
            &user,
            Method::POST,
            "/chat-api/v1/chats",
            Some(json!({ "title": "Prefixed" })),
        )
        .await;

    expect_status(&resp, StatusCode::CREATED);
    let id = chat_id(&resp.json());
    let location = resp.headers.get(http::header::LOCATION).unwrap();
    assert_eq!(
        location.to_str().unwrap(),
        format!("/chat-api/v1/chats/{id}")
    );
}
