#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers build scripted gateway results"
)]

//! Anthropic Files client tests over the fake OAGW gateway: client creation,
//! copy targets and aliases, the multipart upload without `purpose`, the
//! delete and the outcome classification.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::json;

use super::*;
use crate::infra::llm::fake_gateway::{FakeGateway, StaticSystemContext, json_response, lock};

const TENANT: Uuid = Uuid::from_u128(0x7e7a);
const OVERRIDE_TENANT: Uuid = Uuid::from_u128(0x0bad);

fn entry(value: serde_json::Value) -> ProviderEntry {
    serde_json::from_value(value).unwrap()
}

fn providers() -> BTreeMap<String, ProviderEntry> {
    BTreeMap::from([
        (
            "openai".to_owned(),
            entry(json!({
                "kind": "openai_responses",
                "host": "api.openai.com",
                "upstream_alias": "api.openai.com",
                "storage_kind": "openai",
            })),
        ),
        (
            "claude".to_owned(),
            entry(json!({
                "kind": "anthropic_messages",
                "host": "api.anthropic.com",
                "upstream_alias": "mock-anthropic",
                "storage_kind": "openai",
                "rag_provider": "openai",
                "tenant_overrides": {
                    OVERRIDE_TENANT.to_string(): { "upstream_alias": "tenant-anthropic" }
                },
            })),
        ),
    ])
}

fn files(gateway: &Arc<FakeGateway>) -> AnthropicFiles {
    AnthropicFiles::from_providers(
        Arc::clone(gateway) as Arc<dyn ServiceGatewayClientV1>,
        Arc::new(StaticSystemContext::default()),
        &providers(),
    )
    .unwrap()
}

#[test]
fn no_client_without_an_anthropic_entry() {
    let mut providers = providers();
    providers.remove("claude");
    let gateway = Arc::new(FakeGateway::default());
    assert!(
        AnthropicFiles::from_providers(
            gateway,
            Arc::new(StaticSystemContext::default()),
            &providers
        )
        .is_none()
    );
}

#[test]
fn targets_and_aliases() {
    let gateway = Arc::new(FakeGateway::default());
    let files = files(&gateway);
    assert_eq!(
        files.secondary_target("claude", TENANT),
        Some(SecondaryTarget {
            provider_id: "claude".to_owned(),
            provider_kind: "anthropic".to_owned(),
            upstream_alias: "mock-anthropic".to_owned(),
        })
    );
    assert_eq!(
        files
            .secondary_target("claude", OVERRIDE_TENANT)
            .unwrap()
            .upstream_alias,
        "tenant-anthropic"
    );
    assert_eq!(
        files.secondary_target("openai", TENANT),
        None,
        "no copy for other kinds"
    );
    assert_eq!(
        files.upstream_alias(TENANT, "anthropic", None).as_deref(),
        Some("mock-anthropic")
    );
    assert_eq!(
        files
            .upstream_alias(OVERRIDE_TENANT, "anthropic", Some("claude"))
            .as_deref(),
        Some("tenant-anthropic")
    );
    assert_eq!(files.upstream_alias(TENANT, "other", None), None);
    assert_eq!(files.upstream_alias(TENANT, "other", Some("claude")), None);
    assert_eq!(
        files.upstream_alias(TENANT, "anthropic", Some("removed")),
        None,
        "a stored entry that is no longer configured never falls back to another account"
    );
}

/// Two `anthropic_messages` entries on different upstreams.
fn two_anthropic_entries() -> BTreeMap<String, ProviderEntry> {
    let mut providers = providers();
    providers.insert(
        "claude-second".to_owned(),
        entry(json!({
            "kind": "anthropic_messages",
            "host": "api.anthropic.com",
            "upstream_alias": "second-anthropic",
            "storage_kind": "openai",
            "rag_provider": "openai",
            "tenant_overrides": {
                OVERRIDE_TENANT.to_string(): { "upstream_alias": "tenant-second-anthropic" }
            },
        })),
    );
    providers
}

#[test]
fn stored_provider_entry_selects_the_upload_account() {
    let gateway = Arc::new(FakeGateway::default());
    let files = AnthropicFiles::from_providers(
        Arc::clone(&gateway) as Arc<dyn ServiceGatewayClientV1>,
        Arc::new(StaticSystemContext::default()),
        &two_anthropic_entries(),
    )
    .unwrap();
    let target = files.secondary_target("claude-second", TENANT).unwrap();
    assert_eq!(target.upstream_alias, "second-anthropic");
    assert_eq!(
        files
            .upstream_alias(TENANT, "anthropic", Some(&target.provider_id))
            .as_deref(),
        Some("second-anthropic"),
        "the delete reaches the account of the upload, not the first entry"
    );
    assert_eq!(
        files
            .upstream_alias(OVERRIDE_TENANT, "anthropic", Some("claude-second"))
            .as_deref(),
        Some("tenant-second-anthropic")
    );
    assert_eq!(
        files.upstream_alias(TENANT, "anthropic", None).as_deref(),
        Some("mock-anthropic"),
        "a row without a stored entry falls back to the first entry"
    );
}

fn upload() -> FileUpload {
    FileUpload {
        filename: "chat_att.png".to_owned(),
        content_type: "image/png".to_owned(),
        data: Bytes::from_static(b"\x89PNG-bytes"),
    }
}

#[tokio::test]
async fn upload_sends_only_the_file_part_with_the_files_beta() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "id": "file_abc", "type": "file" }),
        &[],
        None,
    )));
    let files = files(&gateway);
    let target = files.secondary_target("claude", TENANT).unwrap();
    let file_id = files
        .upload_secondary_file(&target, upload())
        .await
        .unwrap();
    assert_eq!(file_id, "file_abc");

    let sent = lock(&gateway.proxied)[0].clone();
    assert_eq!(sent.method, Method::POST);
    assert_eq!(sent.uri, "/mock-anthropic/v1/files");
    assert_eq!(sent.headers["anthropic-version"], "2023-06-01");
    assert_eq!(sent.headers["anthropic-beta"], "files-api-2025-04-14");
    let raw = String::from_utf8_lossy(&sent.raw);
    assert!(raw.contains("name=\"file\""), "{raw}");
    assert!(!raw.contains("name=\"purpose\""), "no purpose: {raw}");
    assert_eq!(sent.tenant_id, Uuid::from_u128(0x2222), "S2S context");
}

#[tokio::test]
async fn upload_failures_are_classified() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        529,
        &json!({ "type": "error", "error": { "type": "overloaded_error", "message": "busy" } }),
        &[],
        None,
    )));
    gateway.push_proxy(Ok(json_response(
        400,
        &json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "bad" } }),
        &[],
        None,
    )));
    gateway.push_proxy(Ok(json_response(200, &json!({}), &[], None)));
    let files = files(&gateway);
    let target = files.secondary_target("claude", TENANT).unwrap();
    let transient = files
        .upload_secondary_file(&target, upload())
        .await
        .unwrap_err();
    assert!(transient.is_transient(), "{transient:?}");
    let rejected = files
        .upload_secondary_file(&target, upload())
        .await
        .unwrap_err();
    assert!(!rejected.is_transient(), "{rejected:?}");
    let no_id = files
        .upload_secondary_file(&target, upload())
        .await
        .unwrap_err();
    assert!(!no_id.is_transient(), "{no_id:?}");
}

#[tokio::test]
async fn delete_uses_the_given_alias_and_accepts_404() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "id": "file_abc", "type": "file_deleted" }),
        &[],
        None,
    )));
    gateway.push_proxy(Ok(json_response(404, &json!({}), &[], None)));
    gateway.push_proxy(Ok(json_response(500, &json!({}), &[], None)));
    let files = files(&gateway);
    files
        .delete_secondary_file(TENANT, "file_abc", "anthropic", "mock-anthropic")
        .await
        .unwrap();
    files
        .delete_secondary_file(TENANT, "file_gone", "anthropic", "mock-anthropic")
        .await
        .unwrap();
    let failed = files
        .delete_secondary_file(TENANT, "file_x", "anthropic", "mock-anthropic")
        .await
        .unwrap_err();
    assert!(failed.is_transient());
    let unsupported = files
        .delete_secondary_file(TENANT, "file_x", "other", "mock-anthropic")
        .await
        .unwrap_err();
    assert!(!unsupported.is_transient());

    let proxied = lock(&gateway.proxied).clone();
    assert_eq!(proxied.len(), 3, "an unsupported kind is never proxied");
    assert_eq!(proxied[0].method, Method::DELETE);
    assert_eq!(proxied[0].uri, "/mock-anthropic/v1/files/file_abc");
    assert_eq!(proxied[0].headers["anthropic-version"], "2023-06-01");
}
