//! Built-in `noop` auth plugin.
//!
//! A deliberately empty credential injector: it accepts any configuration and
//! never touches the outbound header map. It exists so that an upstream that
//! needs *no* outbound authentication can still declare the `auth` block
//! explicitly — and so the catalog can advertise it as a real, resolvable
//! plugin rather than a documentation-only entry.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, PluginContext};

/// Auth plugin that injects nothing and accepts every configuration.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuthPlugin;

impl NoopAuthPlugin {
    /// Build the plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn validate_config(&self, _config: &serde_json::Value) -> Result<(), DomainError> {
        // Any configuration is acceptable — there is nothing to validate.
        Ok(())
    }

    async fn authenticate(
        &self,
        _ctx: &PluginContext,
        _config: &serde_json::Value,
        _headers: &mut http::HeaderMap,
    ) -> Result<(), DomainError> {
        // No credential injection: leave the outbound headers untouched.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn injects_nothing_and_accepts_any_config() {
        let plugin = NoopAuthPlugin::new();
        assert_eq!(plugin.name(), "noop");

        for config in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!({ "anything": [1, 2, 3] }),
        ] {
            plugin
                .validate_config(&config)
                .unwrap_or_else(|e| panic!("config {config} must be accepted: {e}"));
        }

        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, http::HeaderValue::from_static("present"));
        plugin
            .authenticate(
                &PluginContext::default(),
                &serde_json::json!({}),
                &mut headers,
            )
            .await
            .expect("noop never fails");
        assert_eq!(
            headers.get(http::header::AUTHORIZATION),
            Some(&http::HeaderValue::from_static("present"))
        );
        assert_eq!(headers.len(), 1, "noop must not add a header");
    }
}
