//! `required_headers` guard plugin (ADR 0009).
//!
//! Checks the *presence* of configured header names, case-insensitively, in the
//! request phase (`required_request_headers`, rejecting with `400`) and in the
//! response phase (`required_response_headers`, rejecting with `502`). Absent or
//! blank configuration makes the phase a no-op (fail-open). Only the first
//! missing header is reported.

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::plugin::{GuardPlugin, PluginConfig, RequestContext, ResponseContext};

/// Stateless presence-check guard.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

/// Splits and normalizes a comma-separated header list, dropping blank entries.
#[must_use]
pub fn parse_required_headers(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn first_missing(headers: &http::HeaderMap, required: &[String]) -> Option<String> {
    required
        .iter()
        .find(|name| !headers.contains_key(name.as_str()))
        .cloned()
}

fn parse_list(config: &PluginConfig, key: &str) -> Vec<String> {
    config
        .string(key)
        .map(parse_required_headers)
        .unwrap_or_default()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        gts::GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(
        &self,
        ctx: &mut RequestContext,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let required = parse_list(config, "required_request_headers");
        if required.is_empty() {
            return Ok(());
        }
        if let Some(missing) = first_missing(&ctx.headers, &required) {
            return Err(DomainError::Validation(format!(
                "required request header `{missing}` is missing"
            )));
        }
        Ok(())
    }

    async fn guard_response(
        &self,
        ctx: &mut ResponseContext<'_>,
        config: &PluginConfig,
    ) -> Result<(), DomainError> {
        let required = parse_list(config, "required_response_headers");
        if required.is_empty() {
            return Ok(());
        }
        if let Some(missing) = first_missing(&ctx.headers, &required) {
            return Err(DomainError::DownstreamRejected(format!(
                "upstream response is missing required header `{missing}`"
            )));
        }
        Ok(())
    }
}
