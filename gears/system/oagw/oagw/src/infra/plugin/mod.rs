// Created: 2026-09-01 by Constructor Tech
//! Plugin traits, registries, the built-ins, and the chain executor.
//!
//! `docs/ADR/0002-plugin-system.md` defines three plugin types with a
//! deterministic execution order: Auth → Guards → Transform(on_request) →
//! upstream → Transform(on_response / on_error). Upstream plugins execute
//! before route plugins; the Control Plane merges the two sets into one
//! ordered [`Chain`](crate::infra::context::Chain) before the executor sees
//! it.

pub mod apikey;
pub mod executor;
pub mod noop;
pub mod oauth2;
pub mod registry;
pub mod request_id;
pub mod required_headers;
pub mod traits;

use crate::infra::credstore::SecretResolver;
use registry::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};

/// The three registries, bundled so a call site passes one thing around.
#[derive(Debug, Clone)]
pub struct Registries {
    /// Credential injection.
    pub auth: AuthPluginRegistry,
    /// Validation and policy.
    pub guards: GuardPluginRegistry,
    /// Request and response mutation.
    pub transforms: TransformPluginRegistry,
}

impl Registries {
    /// The built-in plugins, with the credential resolver they need.
    #[must_use]
    pub fn builtins(
        resolver: SecretResolver,
        http_config: Option<toolkit_http::HttpClientConfig>,
        cache: oauth2::TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(resolver, http_config, cache),
            guards: GuardPluginRegistry::with_builtins(),
            transforms: TransformPluginRegistry::with_builtins(),
        }
    }

    /// Every identifier the three registries know, by kind.
    #[must_use]
    pub fn ids(&self) -> Vec<(&'static str, Vec<String>)> {
        vec![
            (
                "auth",
                self.auth.ids().into_iter().map(String::from).collect(),
            ),
            (
                "guard",
                self.guards.ids().into_iter().map(String::from).collect(),
            ),
            (
                "transform",
                self.transforms
                    .ids()
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
        ]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_listed_by_kind() {
        let registries = Registries::builtins(
            SecretResolver::unlinked(),
            None,
            oauth2::TokenCacheConfig::default(),
        );
        let ids = registries.ids();
        assert_eq!(ids[0].0, "auth");
        assert_eq!(ids[0].1.len(), 4);
        assert_eq!(
            ids[1].1,
            vec![crate::domain::model::builtin_plugins::GUARD_REQUIRED_HEADERS]
        );
        assert_eq!(
            ids[2].1,
            vec![crate::domain::model::builtin_plugins::TRANSFORM_REQUEST_ID]
        );
    }
}
