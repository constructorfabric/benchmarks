//! Provider entry fixtures (`providers.<id>` of the gear configuration).

use crate::config::{MiniChatConfig, ProviderEntry, ProviderKind, StorageKind};

/// The default `openai` entry.
#[must_use]
pub fn openai_entry() -> ProviderEntry {
    MiniChatConfig::default().providers["openai"].clone()
}

/// An Azure `OpenAI` Responses entry on `host` (`storage_kind = azure`,
/// `api_version = 2025-04-01-preview`).
#[must_use]
pub fn azure_entry(host: &str) -> ProviderEntry {
    let mut e = openai_entry();
    host.clone_into(&mut e.host);
    "/openai/v1/responses".clone_into(&mut e.api_path);
    e.storage_kind = StorageKind::Azure;
    e.api_version = Some("2025-04-01-preview".to_owned());
    e
}

/// An `anthropic_messages` entry on `api.anthropic.com` (`/v1/messages`) whose
/// files and vector stores live in the `openai` entry.
#[must_use]
pub fn anthropic_entry() -> ProviderEntry {
    let mut e = openai_entry();
    e.kind = ProviderKind::AnthropicMessages;
    "api.anthropic.com".clone_into(&mut e.host);
    "/v1/messages".clone_into(&mut e.api_path);
    e.auth_config = [
        ("header".to_owned(), "x-api-key".to_owned()),
        ("secret_ref".to_owned(), "cred://anthropic-key".to_owned()),
    ]
    .into();
    e.rag_provider = Some("openai".to_owned());
    e
}
