//! `RequiredHeadersGuardPlugin` — required header enforcement.
//!
//! See ADR-0009: `required_request_headers` / `required_response_headers` are
//! comma-separated header names. A phase with an absent or blank list is a
//! no-op (fail-open); the first missing header is reported. The request phase
//! answers 400, the response phase 502.

use async_trait::async_trait;

use super::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext, config_list,
};

#[allow(dead_code)]
fn _unused(_: PluginError) {}

/// Required-header guard.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

/// Error code emitted when a required header is missing.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        crate::gts::guard_plugin::REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        let required = config_list(&ctx.config, "required_request_headers");
        if required.is_empty() {
            return Ok(GuardDecision::Continue);
        }
        for name in &required {
            match http::HeaderName::try_from(name.as_str()) {
                Ok(name) => {
                    let present = ctx
                        .headers
                        .get(&name)
                        .map(|v| !v.as_bytes().iter().all(|b| b.is_ascii_whitespace()))
                        .unwrap_or(false);
                    if !present {
                        return Ok(GuardDecision::Reject(missing_header(
                            400,
                            name.as_str(),
                            &required.join(", "),
                        )));
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(GuardDecision::Continue)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        let required = config_list(&ctx.config, "required_response_headers");
        if required.is_empty() {
            return Ok(GuardDecision::Continue);
        }
        for name in &required {
            match http::HeaderName::try_from(name.as_str()) {
                Ok(name) => {
                    let present = ctx
                        .headers
                        .get(&name)
                        .map(|v| !v.as_bytes().iter().all(|b| b.is_ascii_whitespace()))
                        .unwrap_or(false);
                    if !present {
                        return Ok(GuardDecision::Reject(missing_header(
                            502,
                            name.as_str(),
                            name.as_str(),
                        )));
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(GuardDecision::Continue)
    }
}

fn missing_header(status: u16, name: &str, configured: &str) -> crate::domain::error::OagwError {
    let mut err = if status == 400 {
        crate::domain::error::OagwError::validation(format!(
            "required request header {name:?} is missing (required_headers={configured})"
        ))
    } else {
        crate::domain::error::OagwError::protocol_error(format!(
            "upstream response is missing required header {name:?}"
        ))
    };
    err = err
        .with_extension("error_code", REQUIRED_HEADER_MISSING)
        .with_extension("header_name", name);
    err
}

impl GuardDecision {
    /// Convenience constructor for tests.
    #[must_use]
    pub fn is_reject(&self) -> bool {
        matches!(self, Self::Reject(_))
    }
}
