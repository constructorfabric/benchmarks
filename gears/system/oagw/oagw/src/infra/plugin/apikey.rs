//! API-key credential injection (ADR 0002 "Built-in Plugins").
//!
//! Configuration (`upstream.auth.config`):
//!
//! | Key | Default | Meaning |
//! |---|---|---|
//! | `name` (or `header_name`) | `x-api-key` | Header carrying the key |
//! | `query` | `false` | Send the key as a query parameter instead |
//! | `param_name` | `api_key` | Query parameter name when `query` is set |
//! | `value` (or `key`, `value_ref`, `secret_ref`) | — | Literal key or `cred://` locator |

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, RequestContext};
use crate::infra::transport::OUTBOUND_QUERY_HEADER;

use super::secret::{config_any, config_flag, config_string, SecretResolver};
use crate::domain::plugin::builtins::AUTH_API_KEY;

/// Registry key of the API-key auth plugin.
pub const API_KEY_PLUGIN_ID: &str = "apikey";
/// Default header an API key is sent in.
pub const DEFAULT_API_KEY_HEADER: &str = "x-api-key";
/// Default query parameter an API key is sent in.
pub const DEFAULT_API_KEY_PARAM: &str = "api_key";

/// Injects a static API key into the outbound request.
///
/// Review evidence (privilege boundary — credential injection):
/// * Guardrail: DESIGN §3.1 "Secret Access Control" + §2.1
///   `principle-cred-isolation` — the key is resolved through
///   [`super::secret::SecretResolver`] and written to the *outbound* request
///   only; it is never logged, never echoed into an error and never added to
///   the response returned to the caller.
/// * Rationale: the credential is injected into `outbound_headers`, which is
///   rebuilt from scratch by the proxy engine, so a caller cannot escalate by
///   pre-setting the injection header on the inbound request.
/// * Validation performed: `apikey_*` tests in `infra/plugin/tests.rs` cover
///   header injection, query injection and the missing-credential failure.
#[derive(Clone)]
pub struct ApiKeyAuthPlugin {
    resolver: std::sync::Arc<dyn SecretResolver>,
}

impl std::fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The resolved credential is never rendered.
        f.debug_struct("ApiKeyAuthPlugin").finish_non_exhaustive()
    }
}

impl ApiKeyAuthPlugin {
    /// Builds the plugin over a credential resolver.
    #[must_use]
    pub fn new(resolver: std::sync::Arc<dyn SecretResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        API_KEY_PLUGIN_ID
    }

    fn plugin_type(&self) -> &'static str {
        AUTH_API_KEY
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let name = config_any(&ctx.config, &["header_name", "name"])
            .unwrap_or_else(|| DEFAULT_API_KEY_HEADER.to_owned())
            .trim()
            .to_ascii_lowercase();
        let reference = config_any(&ctx.config, &["value", "key", "value_ref", "secret_ref"])
            .unwrap_or_default();
        let material = self.resolver.resolve(ctx, &reference).await?;
        if config_flag(&ctx.config, "query", false) {
            let param = config_string(&ctx.config, "param_name")
                .unwrap_or_else(|| DEFAULT_API_KEY_PARAM.to_owned());
            ctx.set_outbound_header(
                OUTBOUND_QUERY_HEADER,
                format!("{param}={material}"),
            );
        } else {
            ctx.set_outbound_header(name, material);
        }
        Ok(())
    }
}
