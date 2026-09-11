//! Built-in `required_headers` guard plugin (ADR-0009).
//!
//! Configuration is read from the binding's `config` as two comma-separated header-name lists,
//! `required_request_headers` and `required_response_headers`. Blank or absent configuration makes
//! the phase a no-op (fail-open). Header names match case-insensitively and only presence is
//! checked; the first missing header is the one reported.

use serde_json::Value;

use crate::domain::gts_helpers;
use crate::domain::plugin::{GuardPlugin, PluginError, ProxyRequest, ProxyResponse};
use super::registry::header_list;

/// Machine-readable code the guard rejects with (ADR-0009).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// The [`GuardPlugin`] enforcing header presence.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait::async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        gts_helpers::GUARD_REQUIRED_HEADERS
    }

    fn plugin_type(&self) -> &'static str {
        "required_headers"
    }

    async fn guard_request(
        &self,
        request: &ProxyRequest,
        config: &Value,
    ) -> Result<(), PluginError> {
        let Some(required) = header_list(config, "required_request_headers") else {
            return Ok(());
        };
        for name in required {
            if request.headers.get(&name).is_none() {
                return Err(PluginError::Guard {
                    code: REQUIRED_HEADER_MISSING.to_string(),
                    message: format!("required request header '{name}' is missing"),
                });
            }
        }
        Ok(())
    }

    async fn guard_response(
        &self,
        response: &ProxyResponse,
        config: &Value,
    ) -> Result<(), PluginError> {
        let Some(required) = header_list(config, "required_response_headers") else {
            return Ok(());
        };
        for name in required {
            if response.headers.get(&name).is_none() {
                return Err(PluginError::ResponseGuard {
                    code: REQUIRED_HEADER_MISSING.to_string(),
                    message: format!("required response header '{name}' is missing"),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "required_headers_guard_tests.rs"]
mod tests;
