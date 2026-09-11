//! Process-lifetime plugin-chain runtime (RF-001).
//!
//! `crate::proxy::engine`'s real call sites (`execute_pre_call`/
//! `execute_post_call`) need three things constructed once per process,
//! never once per request: the built-in [`Registries`], the
//! client-credentials [`TokenCache`], and a `cred_store` client. This
//! module is the single process-lifetime [`OnceLock`] that replaces the
//! narrow adapters' old per-call `Registries::init()`
//! (`crate::plugins::chain`'s pre-RF-001 doc comment).
//!
//! ## The `cred_store` client this round can honestly provide
//!
//! A real, DB-backed `credstore::client::CredStoreLocalClient` needs an
//! `Arc<credstore::domain::secret::service::Service>`, obtainable only
//! through a cross-gear `toolkit::client_hub::ClientHub` handle. This gear's
//! `#[toolkit::gear(...)]` registration (`src/lib.rs`) declares no
//! `deps = [credstore]` edge, and `RestApiCapability::register_rest`
//! (`src/lib.rs`) does not thread a `GearCtx`-derived client through
//! `src/api/rest/proxy.rs`'s `handle`/`register_routes` into
//! [`crate::proxy::engine::ProxyDeps`] -- exactly the same documented gap
//! `crate::proxy::hierarchy::NoTenantHierarchy` records for
//! `tenant-resolver-sdk` ("this gear has no `GearCtx`/hub handle reachable
//! from a REST handler to a real ... client in this round"). Closing it
//! needs changes to `src/lib.rs` (shared) and `src/api/rest/proxy.rs` (the
//! sibling's file-ownership), so it is **reported, not made**, here.
//!
//! In production (no `test-utils` feature) this resolves to [`NoCredStore`]:
//! every `secret_ref` fails closed with "not accessible", which an
//! `apikey`/`oauth2_*` auth binding maps to a genuine `401
//! AuthenticationFailed` -- a loud, correct failure, a strict improvement
//! over the pre-RF-001 state where the credential injection code never ran
//! at all and the request silently proceeded without it. Under
//! `test-utils` (this crate's own self-referential dev-dependency, active
//! for every `#[cfg(test)]` unit test and every `tests/*.rs` integration
//! test), this resolves to [`test_support::SharedTestCredStore`], a real
//! in-process secret registry test code can seed through
//! [`test_support::register_secret`] -- the seam that lets
//! `tests/plugin_execution.rs` prove a configured credential genuinely
//! reaches a mocked upstream through the real, unmodified production
//! router.

use std::sync::OnceLock;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef};
use toolkit_security::SecurityContext;

use super::registry::Registries;
use super::token_cache::TokenCache;

/// Everything the real chain executor needs that must be constructed once
/// per process, not once per request (`crate::proxy::engine`'s call sites
/// share this handle across every request).
pub(crate) struct ChainRuntime {
    pub registries: Registries,
    pub token_cache: TokenCache,
    pub credstore: Box<dyn CredStoreClientV1>,
}

/// The shared, process-lifetime instance, sized on first use from the
/// gear's resolved `token_cache_capacity` (RF-006, `crate::config::OagwConfig`).
/// Later calls' `token_cache_capacity` argument is ignored once
/// constructed -- exactly the bucket-sticky-at-seed-time pattern RF-003
/// applies to rate-limit buckets, for the same reason: a process-lifetime
/// resource cannot re-size itself out from under in-flight state on every
/// call.
pub(crate) fn chain_runtime(token_cache_capacity: usize) -> &'static ChainRuntime {
    static RUNTIME: OnceLock<ChainRuntime> = OnceLock::new();
    RUNTIME.get_or_init(|| ChainRuntime {
        registries: Registries::init(),
        token_cache: TokenCache::new(token_cache_capacity),
        credstore: production_credstore(),
    })
}

#[cfg(not(feature = "test-utils"))]
fn production_credstore() -> Box<dyn CredStoreClientV1> {
    Box::new(NoCredStore)
}

/// See the module doc comment: the fail-closed stand-in used whenever no
/// real cross-gear `cred_store` client is reachable (every non-test build
/// this round). Not itself `cfg`-gated (unlike the `production_credstore`
/// selector above) so this crate's own test suite -- which always builds
/// with `test-utils` active via the self-referential dev-dependency -- can
/// still unit-test it directly.
///
/// `#[allow(dead_code)]`: `production_credstore` only *constructs* this in
/// a `not(feature = "test-utils")` build -- a real production build this
/// workspace's own `cargo test`/`cargo clippy --all-targets` invocations
/// never produce, since both always activate `test-utils` (this crate's
/// own self-referential dev-dependency). The reserved-entry-point
/// situation is the same one `crate::proxy::engine::flush_resolved_config_cache`
/// documents for its own extension point.
#[allow(dead_code)]
pub(crate) struct NoCredStore;

#[async_trait]
impl CredStoreClientV1 for NoCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        Ok(None)
    }
}

#[cfg(feature = "test-utils")]
fn production_credstore() -> Box<dyn CredStoreClientV1> {
    Box::new(test_support::SharedTestCredStore)
}

/// Test-only in-process secret registry, shared process-lifetime across
/// every `#[cfg(test)]` unit test and every `tests/*.rs` integration test in
/// this crate. See the module doc comment for why this is the credential
/// seam a black-box test drives, rather than a `src/api/rest/**` wiring
/// change.
#[cfg(feature = "test-utils")]
pub mod test_support {
    use std::sync::OnceLock;

    use async_trait::async_trait;
    use credstore_sdk::{
        CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef, SecretType, SecretValue,
        SharingMode, TenantId,
    };
    use dashmap::DashMap;
    use toolkit_security::SecurityContext;

    fn registry() -> &'static DashMap<String, Vec<u8>> {
        static REGISTRY: OnceLock<DashMap<String, Vec<u8>>> = OnceLock::new();
        REGISTRY.get_or_init(DashMap::new)
    }

    /// Seed a secret so an `apikey`/`oauth2_*` auth binding's `secret_ref`
    /// resolves for real through the production router -- test-only, and
    /// process-lifetime (shared, never reset) so callers should use a
    /// unique key per test (e.g. a fresh `Uuid`) rather than relying on
    /// isolation between test cases.
    pub fn register_secret(key: &str, value: &str) {
        registry().insert(key.to_owned(), value.as_bytes().to_vec());
    }

    /// Build a canned response wrapping `value` with placeholder metadata,
    /// mirroring `credstore_sdk::test_util::MockCredStoreClient`'s own
    /// (test-only, not reachable from here) helper of the same shape.
    fn response(value: Vec<u8>) -> GetSecretResponse {
        GetSecretResponse {
            value: SecretValue::new(value),
            id: uuid::Uuid::nil(),
            owner_tenant_id: TenantId::nil(),
            sharing: SharingMode::default(),
            is_inherited: false,
            version: 1,
            secret_type: SecretType::generic().gts_id().to_owned(),
            expires_at: None,
        }
    }

    pub(crate) struct SharedTestCredStore;

    #[async_trait]
    impl CredStoreClientV1 for SharedTestCredStore {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            key: &SecretRef,
        ) -> Result<Option<GetSecretResponse>, CredStoreError> {
            Ok(registry()
                .get(key.as_ref())
                .map(|value| response(value.clone())))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn production_credstore_fails_closed_rather_than_silently_succeeding() {
        let store = NoCredStore;
        let secret_ref = SecretRef::new("anything").unwrap();
        let result = store.get(&ctx(), &secret_ref).await.unwrap();
        assert!(
            result.is_none(),
            "with no real cred_store wired, every reference must resolve to \
             'not accessible' (a loud 401), never a silently fabricated secret"
        );
    }
}
