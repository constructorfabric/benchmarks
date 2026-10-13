#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use super::{OagwStorage, indexing_status, is_safe_segment, object_id, storage_uri};
use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::attachment::{IndexingStatus, StorageFlavor, StorageGateway, StorageRoute};
use crate::test_support::{FakeOagw, FakeProvider};

fn anthropic_entry() -> ProviderEntry {
    ProviderEntry {
        kind: ProviderKind::AnthropicMessages,
        host: "anthropic.test".to_owned(),
        rag_provider: Some("openai".to_owned()),
        ..ProviderEntry::default_openai()
    }
}

fn storage(entries: Vec<(&str, ProviderEntry)>) -> OagwStorage {
    OagwStorage::new(
        Arc::new(FakeOagw::new(FakeProvider::default())),
        entries
            .into_iter()
            .map(|(id, entry)| (id.to_owned(), entry))
            .collect::<HashMap<_, _>>(),
    )
}

fn route(flavor: StorageFlavor) -> StorageRoute {
    StorageRoute {
        provider_id: "p".to_owned(),
        backend: "p".to_owned(),
        alias: "host.test".to_owned(),
        flavor,
    }
}

#[test]
fn openai_storage_is_served_under_v1_and_azure_under_openai_with_the_api_version() {
    assert_eq!(
        storage_uri(&route(StorageFlavor::Openai), "/files"),
        "/host.test/v1/files"
    );
    assert_eq!(
        storage_uri(
            &route(StorageFlavor::Azure {
                api_version: "2025-03-01".to_owned()
            }),
            "/vector_stores/vs_1/files"
        ),
        "/host.test/openai/vector_stores/vs_1/files?api-version=2025-03-01"
    );
}

#[test]
fn a_missing_status_is_in_progress_and_unknown_statuses_are_failures() {
    assert_eq!(indexing_status(&json!({})), IndexingStatus::InProgress);
    assert_eq!(
        indexing_status(&json!({ "status": "in_progress" })),
        IndexingStatus::InProgress
    );
    assert_eq!(
        indexing_status(&json!({ "status": "completed" })),
        IndexingStatus::Completed
    );
    for status in ["failed", "cancelled", "expired"] {
        assert_eq!(
            indexing_status(&json!({ "status": status })),
            IndexingStatus::Failed(status.to_owned())
        );
    }
}

#[test]
fn a_chat_stores_its_files_with_its_own_provider_and_gets_no_secondary_copy() {
    let storage = storage(vec![("openai", ProviderEntry::default_openai())]);

    let chat = storage.chat_storage(Uuid::new_v4(), "openai").unwrap();

    assert_eq!(chat.primary.provider_id, "openai");
    assert_eq!(chat.primary.backend, "openai");
    assert_eq!(chat.primary.flavor, StorageFlavor::Openai);
    assert!(chat.secondary.is_none());
}

#[test]
fn an_anthropic_chat_stores_through_the_rag_provider_and_copies_images_to_anthropic() {
    let storage = storage(vec![
        ("openai", ProviderEntry::default_openai()),
        ("claude", anthropic_entry()),
    ]);

    let chat = storage.chat_storage(Uuid::new_v4(), "claude").unwrap();

    assert_eq!(chat.primary.provider_id, "openai");
    assert_eq!(chat.primary.alias, "api.openai.com");
    let secondary = chat.secondary.unwrap();
    assert_eq!(secondary.alias, "anthropic.test");
    assert_eq!(secondary.provider_kind, "anthropic");
}

#[test]
fn an_unconfigured_provider_or_rag_provider_is_a_policy_error() {
    let missing_rag = ProviderEntry {
        rag_provider: Some("gone".to_owned()),
        ..anthropic_entry()
    };
    let storage = storage(vec![("claude", missing_rag)]);

    assert!(storage.chat_storage(Uuid::new_v4(), "unknown").is_err());
    assert!(storage.chat_storage(Uuid::new_v4(), "claude").is_err());
}

#[test]
fn azure_storage_carries_its_api_version() {
    let azure = ProviderEntry {
        storage_kind: StorageKind::Azure,
        api_version: Some("2025-03-01".to_owned()),
        ..ProviderEntry::default_openai()
    };
    let storage = storage(vec![("azure", azure)]);

    let chat = storage.chat_storage(Uuid::new_v4(), "azure").unwrap();

    assert_eq!(
        chat.primary.flavor,
        StorageFlavor::Azure {
            api_version: "2025-03-01".to_owned()
        }
    );
}

#[test]
fn a_stored_backend_label_maps_back_to_its_provider() {
    let labelled = ProviderEntry {
        storage_backend: Some("eu-files".to_owned()),
        ..ProviderEntry::default_openai()
    };
    let storage = storage(vec![
        ("openai", ProviderEntry::default_openai()),
        ("labelled", labelled),
    ]);
    let tenant = Uuid::new_v4();

    assert_eq!(
        storage
            .route_for_backend(tenant, "openai")
            .unwrap()
            .provider_id,
        "openai"
    );
    assert_eq!(
        storage
            .route_for_backend(tenant, "eu-files")
            .unwrap()
            .provider_id,
        "labelled"
    );
    assert!(storage.route_for_backend(tenant, "nowhere").is_err());
}

#[test]
fn identifiers_that_could_change_a_request_path_are_refused() {
    for safe in [
        "file-abc123",
        "vs_0123",
        "file_011sec",
        "127.0.0.1:18078",
        "assistant-1",
    ] {
        assert!(is_safe_segment(safe), "{safe}");
    }
    for unsafe_value in [
        "", ".", "..", "a/b", "../x", "a?b", "a#b", "a b", "a\\b", "x%2F",
    ] {
        assert!(!is_safe_segment(unsafe_value), "{unsafe_value}");
    }
    assert!(object_id(&json!({ "id": "file-1" })).is_ok());
    assert!(object_id(&json!({ "id": "file-1/../../vector_stores/vs" })).is_err());
    assert!(object_id(&json!({ "id": "" })).is_err());
    assert!(object_id(&json!({})).is_err());
}
