//! `required_headers.v1` — request/response required-header enforcement
//! (ADR 0009).
//!
//! A stateless guard that checks for the presence of configured header names
//! (`required_request_headers` / `required_response_headers`) and rejects with
//! a phase-specific status code on the first missing header. Fail-open when a
//! phase is unconfigured or blank (ADR 0009 "Decision Flow").

use http::HeaderMap;

use crate::domain::plugin::{
    GuardContext, GuardError, GuardPhase, GuardPlugin, REQUIRED_HEADERS_GUARD_PLUGIN_ID, cfg_string,
};

/// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`
///
/// Config keys (ADR 0009):
///
/// | Key | Required | Description |
/// |---|---|---|
/// | `required_request_headers` | no | Comma-separated header names checked in `guard_request`; absent or blank → request phase is a no-op |
/// | `required_response_headers` | no | Comma-separated header names checked in `guard_response`; absent or blank → response phase is a no-op |
///
/// Header names are matched case-insensitively; only presence is checked, not
/// value. Only the first missing header is reported per rejection.
pub struct RequiredHeadersGuardPlugin;

/// Parse one comma-separated config value into lowercased, trimmed, non-empty
/// header names.
fn parse_header_list(config: &serde_json::Value, key: &str) -> Vec<String> {
    cfg_string(config, key)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
}

/// Check each required name (in order) against `headers`; reject on the first
/// missing one (case-insensitive presence only).
fn guard_headers(
    required: &[String],
    headers: &HeaderMap,
    phase: GuardPhase,
) -> Result<(), GuardError> {
    if required.is_empty() {
        return Ok(()); // fail-open when unconfigured/blank
    }
    for name in required {
        if !headers.contains_key(name.as_str()) {
            return Err(GuardError::RequiredHeaderMissing {
                phase,
                header: name.clone(),
            });
        }
    }
    Ok(())
}

impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    fn guard_request(&self, ctx: &GuardContext<'_>) -> Result<(), GuardError> {
        let required = parse_header_list(ctx.config, "required_request_headers");
        guard_headers(&required, ctx.headers, GuardPhase::Request)
    }

    fn guard_response(&self, ctx: &GuardContext<'_>) -> Result<(), GuardError> {
        let required = parse_header_list(ctx.config, "required_response_headers");
        guard_headers(&required, ctx.headers, GuardPhase::Response)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use http::HeaderValue;
    use serde_json::json;
    use uuid::Uuid;

    fn sec_ctx() -> toolkit_security::SecurityContext {
        toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::from_u128(1))
            .build()
            .unwrap()
    }

    /// Run a request-phase guard with the given config/headers.
    fn req(
        plugin: &RequiredHeadersGuardPlugin,
        config: &serde_json::Value,
        headers: &HeaderMap,
    ) -> Result<(), GuardError> {
        let sec = sec_ctx();
        let ctx = GuardContext {
            security_context: Some(&sec),
            config,
            headers,
        };
        plugin.guard_request(&ctx)
    }

    /// Run a response-phase guard with the given config/headers.
    fn resp(
        plugin: &RequiredHeadersGuardPlugin,
        config: &serde_json::Value,
        headers: &HeaderMap,
    ) -> Result<(), GuardError> {
        let sec = sec_ctx();
        let ctx = GuardContext {
            security_context: Some(&sec),
            config,
            headers,
        };
        plugin.guard_response(&ctx)
    }

    #[test]
    fn unconfigured_phases_fail_open() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("*/*"));
        // No config keys at all.
        assert!(req(&plugin, &json!({}), &headers).is_ok());
        assert!(resp(&plugin, &json!({}), &headers).is_ok());
        // Blank config values (even all-blank comma lists) are no-ops.
        let blank = json!({
            "required_request_headers": ", , ,",
            "required_response_headers": ""
        });
        assert!(req(&plugin, &blank, &headers).is_ok());
        assert!(resp(&plugin, &blank, &headers).is_ok());
    }

    #[test]
    fn request_phase_rejects_first_missing_with_400_semantics() {
        let plugin = RequiredHeadersGuardPlugin;
        let config = json!({ "required_request_headers": "x-correlation-id, accept" });
        let mut headers = HeaderMap::new();
        // Both present (mixed case on the configured name) → allow.
        headers.insert("X-Correlation-Id", HeaderValue::from_static("abc"));
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        assert!(req(&plugin, &config, &headers).is_ok());

        // First missing → request phase error naming only that header.
        headers.remove("accept");
        let err = req(&plugin, &config, &headers).unwrap_err();
        match err {
            GuardError::RequiredHeaderMissing { phase, header } => {
                assert_eq!(phase, GuardPhase::Request);
                assert_eq!(header, "accept");
            }
        }
    }

    #[test]
    fn response_phase_rejects_missing_with_502_semantics() {
        let plugin = RequiredHeadersGuardPlugin;
        let config = json!({ "required_response_headers": "content-type" });
        let bare = HeaderMap::new();
        assert!(resp(&plugin, &config, &bare).is_err());
        let mut with_ct = HeaderMap::new();
        with_ct.insert("Content-Type", HeaderValue::from_static("text/plain"));
        assert!(resp(&plugin, &config, &with_ct).is_ok());
    }

    #[test]
    fn naming_is_case_insensitive_and_only_first_missing_reported() {
        let plugin = RequiredHeadersGuardPlugin;
        // Two missing names: only the first ("x-a") is reported.
        let config = json!({ "required_request_headers": "X-A, x-b" });
        let err = req(&plugin, &config, &HeaderMap::new()).unwrap_err();
        match err {
            GuardError::RequiredHeaderMissing { header, .. } => assert_eq!(header, "x-a"),
        }
    }
}
