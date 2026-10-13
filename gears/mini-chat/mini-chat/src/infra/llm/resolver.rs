//! Maps `(provider_id, tenant)` to the OAGW alias and request paths of a provider entry.

use std::collections::BTreeMap;

use uuid::Uuid;

use super::{ProviderKind, StorageKind};
use crate::config::{ProviderEntry, TenantOverride};
use crate::domain::error::DomainError;
use crate::infra::storage::knowledge::KnowledgeTarget;

/// Where chat requests of a provider go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTarget {
    pub provider_id: String,
    pub kind: ProviderKind,
    pub alias: String,
    /// The configured `api_path`, with the `{model}` placeholder and the query still in it.
    pub api_path_template: String,
}

impl ChatTarget {
    /// `/{alias}{api_path}` with `{model}` replaced by `provider_model_id`.
    #[must_use]
    pub fn uri_for(&self, provider_model_id: &str) -> String {
        format!(
            "/{}{}",
            self.alias,
            self.api_path_template.replace("{model}", provider_model_id)
        )
    }
}

/// Where file and vector-store requests of a provider go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    /// The provider that serves storage (the `rag_provider` target when there is one).
    pub provider_id: String,
    pub storage_kind: StorageKind,
    pub alias: String,
    pub api_version: Option<String>,
    /// Label stored with attachments (`storage_backend`, or the provider id).
    pub storage_backend: String,
}

impl StorageTarget {
    /// `/{alias}{prefix}{path}`; Azure adds `api-version`. `path` starts with `/`, e.g. `/files`.
    #[must_use]
    pub fn uri(&self, path: &str) -> String {
        let prefix = storage_prefix(self.storage_kind);
        let mut uri = format!("/{}{prefix}{path}", self.alias);
        if self.storage_kind == StorageKind::Azure
            && let Some(version) = &self.api_version
        {
            uri.push(if path.contains('?') { '&' } else { '?' });
            uri.push_str("api-version=");
            uri.push_str(version);
        }
        uri
    }
}

/// Route prefix of the file / vector-store API.
#[must_use]
pub fn storage_prefix(kind: StorageKind) -> &'static str {
    match kind {
        StorageKind::Openai => "/v1",
        StorageKind::Azure => "/openai",
    }
}

/// Resolves provider ids to targets from the configured entries.
#[derive(Debug, Clone)]
pub struct ProviderResolver {
    providers: BTreeMap<String, ProviderEntry>,
}

impl ProviderResolver {
    #[must_use]
    pub fn new(providers: BTreeMap<String, ProviderEntry>) -> Self {
        Self { providers }
    }

    fn entry(&self, provider_id: &str) -> Result<&ProviderEntry, DomainError> {
        self.providers
            .get(provider_id)
            .ok_or_else(|| DomainError::Internal(format!("unknown provider '{provider_id}'")))
    }

    /// Chat target of `provider_id` for `tenant_id` (the tenant override's alias when one exists).
    ///
    /// # Errors
    /// `Internal` for an unknown provider id.
    pub fn chat_target(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<ChatTarget, DomainError> {
        let entry = self.entry(provider_id)?;
        Ok(ChatTarget {
            provider_id: provider_id.to_owned(),
            kind: entry.kind,
            alias: entry.alias(tenant_override(entry, tenant_id)),
            api_path_template: entry.api_path.clone(),
        })
    }

    /// Storage target for chats served by `provider_id`: the entry named by its `rag_provider`,
    /// else the entry itself.
    ///
    /// # Errors
    /// `Internal` for an unknown provider or when the target has no `storage_kind`.
    pub fn storage_target(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<StorageTarget, DomainError> {
        let entry = self.entry(provider_id)?;
        let target_id = entry.rag_provider.as_deref().unwrap_or(provider_id);
        self.storage_of(target_id, tenant_id)
    }

    /// Storage target whose `storage_backend` label (or provider id when unset) is `backend`.
    ///
    /// # Errors
    /// `Internal` when no storage-capable provider carries that label.
    pub fn storage_by_backend(
        &self,
        backend: &str,
        tenant_id: Uuid,
    ) -> Result<StorageTarget, DomainError> {
        let id = self
            .providers
            .iter()
            .find(|(id, e)| e.storage_kind.is_some() && backend_label(id, e) == backend)
            .map(|(id, _)| id.as_str())
            .ok_or_else(|| DomainError::Internal(format!("unknown storage backend '{backend}'")))?;
        self.storage_of(id, tenant_id)
    }

    /// Knowledge search target of provider `provider_id` for `tenant_id` (DESIGN "Knowledge
    /// Search" enablement): the entry must exist, be of kind `openai_responses` or
    /// `anthropic_messages` and have a non-empty `api_version`.
    ///
    /// # Errors
    /// Why knowledge search is off for the request.
    pub fn knowledge_target(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<KnowledgeTarget, String> {
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| format!("unknown knowledge provider '{provider_id}'"))?;
        if !matches!(
            entry.kind,
            ProviderKind::OpenaiResponses | ProviderKind::AnthropicMessages
        ) {
            return Err(format!(
                "knowledge provider '{provider_id}' is not of kind openai_responses or anthropic_messages"
            ));
        }
        let api_version = entry
            .api_version
            .clone()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("knowledge provider '{provider_id}' has no api_version"))?;
        let alias = entry.alias(tenant_override(entry, tenant_id));
        if alias.is_empty() {
            return Err(format!(
                "knowledge provider '{provider_id}' has no upstream alias"
            ));
        }
        Ok(KnowledgeTarget { alias, api_version })
    }

    /// OAGW alias of the Anthropic Files API for `tenant_id`: the alias of the
    /// `anthropic_messages` entries when they all share one (`None` without such an entry, or
    /// when several entries disagree and the owner of a file cannot be told).
    #[must_use]
    pub fn anthropic_files_alias(&self, tenant_id: Uuid) -> Option<String> {
        let mut aliases = self
            .providers
            .values()
            .filter(|e| e.kind == ProviderKind::AnthropicMessages)
            .map(|e| e.alias(tenant_override(e, tenant_id)));
        let first = aliases.next()?;
        aliases.all(|a| a == first).then_some(first)
    }

    fn storage_of(&self, id: &str, tenant_id: Uuid) -> Result<StorageTarget, DomainError> {
        let entry = self.entry(id)?;
        let storage_kind = entry
            .storage_kind
            .ok_or_else(|| DomainError::Internal(format!("provider '{id}' has no storage_kind")))?;
        Ok(StorageTarget {
            provider_id: id.to_owned(),
            storage_kind,
            alias: entry.alias(tenant_override(entry, tenant_id)),
            api_version: entry.api_version.clone(),
            storage_backend: backend_label(id, entry),
        })
    }
}

fn backend_label(id: &str, entry: &ProviderEntry) -> String {
    entry
        .storage_backend
        .clone()
        .unwrap_or_else(|| id.to_owned())
}

/// The override whose key names `tenant_id` (UUID in any textual form).
fn tenant_override(entry: &ProviderEntry, tenant_id: Uuid) -> Option<&TenantOverride> {
    entry.tenant_overrides.iter().find_map(|(key, ov)| {
        let matches = Uuid::parse_str(key.trim()).map_or_else(
            |_| key.eq_ignore_ascii_case(&tenant_id.to_string()),
            |id| id == tenant_id,
        );
        matches.then_some(ov)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::app::test_provider;

    fn tenant(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn azure() -> ProviderEntry {
        ProviderEntry {
            host: "res.openai.azure.com".to_owned(),
            port: None,
            use_http: false,
            api_path: "/openai/v1/responses?api-version=2025-03-01-preview".to_owned(),
            storage_kind: Some(StorageKind::Azure),
            api_version: Some("2025-03-01-preview".to_owned()),
            ..test_provider()
        }
    }

    fn resolver() -> ProviderResolver {
        let anthropic = ProviderEntry {
            kind: ProviderKind::AnthropicMessages,
            host: "api.anthropic.com".to_owned(),
            port: None,
            use_http: false,
            api_path: "/v1/messages".to_owned(),
            storage_kind: None,
            rag_provider: Some("azure".to_owned()),
            ..test_provider()
        };
        let mut openai = test_provider();
        openai.tenant_overrides.insert(
            tenant(7).to_string(),
            TenantOverride {
                host: Some("eu.example.com".to_owned()),
                ..TenantOverride::default()
            },
        );
        ProviderResolver::new(BTreeMap::from([
            ("openai".to_owned(), openai),
            ("azure".to_owned(), azure()),
            ("anthropic".to_owned(), anthropic),
        ]))
    }

    #[test]
    fn resolver_uris() {
        let r = resolver();
        let chat = r.chat_target("openai", tenant(1)).unwrap();
        assert_eq!(chat.kind, ProviderKind::OpenaiResponses);
        assert_eq!(chat.uri_for("gpt-4.1"), "/127.0.0.1/v1/responses");

        let storage = r.storage_target("azure", tenant(1)).unwrap();
        assert_eq!(storage.storage_kind, StorageKind::Azure);
        assert_eq!(
            storage.uri("/files"),
            "/res.openai.azure.com/openai/files?api-version=2025-03-01-preview"
        );
        assert_eq!(
            storage.uri("/files?limit=1"),
            "/res.openai.azure.com/openai/files?limit=1&api-version=2025-03-01-preview"
        );
        let openai_storage = r.storage_target("openai", tenant(1)).unwrap();
        assert_eq!(
            openai_storage.uri("/vector_stores"),
            "/127.0.0.1/v1/vector_stores"
        );
        assert_eq!(openai_storage.storage_backend, "openai");
    }

    #[test]
    fn model_placeholder_is_replaced_in_the_template() {
        let mut entry = test_provider();
        entry.api_path = "/openai/deployments/{model}/chat/completions".to_owned();
        let r = ProviderResolver::new(BTreeMap::from([("p".to_owned(), entry)]));
        assert_eq!(
            r.chat_target("p", tenant(1)).unwrap().uri_for("gpt4o"),
            "/127.0.0.1/openai/deployments/gpt4o/chat/completions"
        );
    }

    #[test]
    fn tenant_override_selects_override_alias_for_that_tenant_only() {
        let r = resolver();
        assert_eq!(
            r.chat_target("openai", tenant(7)).unwrap().alias,
            "eu.example.com:9",
            "override host with the entry's non-standard port"
        );
        assert_eq!(
            r.chat_target("openai", tenant(8)).unwrap().alias,
            "127.0.0.1"
        );
        assert_eq!(
            r.storage_target("openai", tenant(7)).unwrap().alias,
            "eu.example.com:9"
        );
    }

    #[test]
    fn rag_provider_is_followed_by_storage_target_only() {
        let r = resolver();
        let chat = r.chat_target("anthropic", tenant(1)).unwrap();
        assert_eq!(chat.alias, "api.anthropic.com");
        let storage = r.storage_target("anthropic", tenant(1)).unwrap();
        assert_eq!(storage.provider_id, "azure");
        assert_eq!(storage.alias, "res.openai.azure.com");
    }

    #[test]
    fn storage_by_backend_matches_label_or_provider_id() {
        let mut openai = test_provider();
        openai.storage_backend = Some("main".to_owned());
        let r = ProviderResolver::new(BTreeMap::from([
            ("openai".to_owned(), openai),
            ("azure".to_owned(), azure()),
        ]));
        assert_eq!(
            r.storage_by_backend("main", tenant(1)).unwrap().provider_id,
            "openai"
        );
        assert_eq!(
            r.storage_by_backend("azure", tenant(1))
                .unwrap()
                .provider_id,
            "azure"
        );
        assert!(matches!(
            r.storage_by_backend("openai", tenant(1)),
            Err(DomainError::Internal(_))
        ));
    }

    #[test]
    fn misconfigured_storage_fails_cleanly() {
        let mut plain = test_provider();
        plain.storage_kind = None;
        let mut selfref = test_provider();
        selfref.rag_provider = Some("selfref".to_owned());
        selfref.storage_kind = None;
        let r = ProviderResolver::new(BTreeMap::from([
            ("plain".to_owned(), plain),
            ("selfref".to_owned(), selfref),
        ]));
        for id in ["plain", "selfref", "missing"] {
            assert!(
                matches!(
                    r.storage_target(id, tenant(1)),
                    Err(DomainError::Internal(_))
                ),
                "{id}"
            );
        }
        assert!(matches!(
            r.chat_target("missing", tenant(1)),
            Err(DomainError::Internal(_))
        ));
    }

    #[test]
    fn knowledge_target_and_anthropic_files_alias() {
        let r = resolver();
        assert_eq!(
            r.knowledge_target("azure", tenant(1)),
            Ok(KnowledgeTarget {
                alias: "res.openai.azure.com".to_owned(),
                api_version: "2025-03-01-preview".to_owned(),
            })
        );
        // no api_version, unknown provider
        assert!(r.knowledge_target("openai", tenant(1)).is_err());
        assert!(r.knowledge_target("missing", tenant(1)).is_err());
        let mut vllm = azure();
        vllm.kind = ProviderKind::VllmResponses;
        let r2 = ProviderResolver::new(BTreeMap::from([("v".to_owned(), vllm)]));
        assert!(r2.knowledge_target("v", tenant(1)).is_err(), "wrong kind");

        assert_eq!(
            r.anthropic_files_alias(tenant(1)).as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(
            r2.anthropic_files_alias(tenant(1)),
            None,
            "no anthropic entry"
        );
        // two Anthropic entries with different aliases: the owner of a file is unknown
        let anthropic = |host: &str| ProviderEntry {
            kind: ProviderKind::AnthropicMessages,
            host: host.to_owned(),
            port: None,
            use_http: false,
            storage_kind: None,
            rag_provider: Some("azure".to_owned()),
            ..test_provider()
        };
        let r3 = ProviderResolver::new(BTreeMap::from([
            ("a".to_owned(), anthropic("a.example.com")),
            ("b".to_owned(), anthropic("b.example.com")),
        ]));
        assert_eq!(r3.anthropic_files_alias(tenant(1)), None);
    }
}
