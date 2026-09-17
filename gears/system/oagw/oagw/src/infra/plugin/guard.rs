//! Built-in guard plugin — `required_headers` (ADR 0009, DoD
//! `cpt-cf-oagw-dod-plugin-system-builtins`, algorithm
//! `cpt-cf-oagw-algo-plugin-system-required-headers`).
//!
//! A stateless presence check over request and response headers.  Both
//! phases are independently configurable and both fail open when the phase's
//! list is absent or blank after trimming (steps `inst-ps-rh-config`,
//! `inst-ps-rh-failopen`).

use async_trait::async_trait;

use crate::domain::plugin::{
    GuardDecision, GuardPhase, GuardPlugin, GuardRejection, RequestContext, ResponseContext,
};

/// `required_headers` — enforces the presence of configured request/response
/// headers (steps `inst-ps-rh-normalize` .. `inst-ps-rh-reject`).
///
/// Config keys (`ctx.config`):
///
/// | Key | Required | Description |
/// |-----|----------|-------------|
/// | `required_request_headers` | No | Comma-separated header names checked in `guard_request`; absent or blank → phase no-op |
/// | `required_response_headers` | No | Comma-separated header names checked in `guard_response`; absent or blank → phase no-op |
///
/// Matches case-insensitively on presence only (values are never validated).
/// The first missing name rejects: request phase 400 `REQUIRED_HEADER_MISSING`,
/// response phase 502 `REQUIRED_HEADER_MISSING`.
#[derive(Debug, Clone, Default)]
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// The guard body shared by both phases: parses the configured list,
    /// fails open when blank, and rejects on the first missing header.
    fn check(
        &self,
        list: Option<&str>,
        headers: &crate::domain::plugin::Headers,
        phase: GuardPhase,
        status: u16,
    ) -> GuardDecision {
        // Fail-open: absent or blank config (steps inst-ps-rh-config /
        // inst-ps-rh-failopen).
        let raw = match list {
            Some(raw) if !raw.trim().is_empty() => raw,
            _ => return GuardDecision::Allow,
        };
        // Normalize: split on commas, trim, lowercase, drop empties (step
        // inst-ps-rh-normalize).
        for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            // Case-insensitive presence only (step inst-ps-rh-scan).
            if !headers.contains(name) {
                // First missing name rejects (step inst-ps-rh-reject).
                return GuardDecision::Reject(GuardRejection {
                    phase,
                    status,
                    code: "REQUIRED_HEADER_MISSING".to_owned(),
                    detail: name.to_owned(),
                });
            }
        }
        GuardDecision::Allow
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::ids::REQUIRED_HEADERS_GUARD
    }

    async fn guard_request(&self, ctx: &RequestContext) -> GuardDecision {
        let list = ctx
            .config
            .get("required_request_headers")
            .and_then(serde_json::Value::as_str);
        self.check(list, &ctx.headers, GuardPhase::Request, 400)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> GuardDecision {
        let list = ctx
            .config
            .get("required_response_headers")
            .and_then(serde_json::Value::as_str);
        self.check(list, &ctx.headers, GuardPhase::Response, 502)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::Headers;
    use crate::domain::plugin::ids::REQUIRED_HEADERS_GUARD;

    fn guard() -> RequiredHeadersGuardPlugin {
        RequiredHeadersGuardPlugin
    }

    #[tokio::test]
    async fn absent_or_blank_config_is_fail_open() {
        let g = guard();
        for config in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "required_request_headers": "" }),
            serde_json::json!({ "required_request_headers": " , , " }),
        ] {
            let ctx = RequestContext {
                headers: Headers::new(),
                config,
                ..RequestContext::default()
            };
            let d = g.guard_request(&ctx).await;
            assert!(d.allow(), "fail-open expected for {ctx:?}");
        }
    }

    #[tokio::test]
    async fn request_phase_rejects_first_missing_header_400() {
        let g = guard();
        let mut ctx = RequestContext {
            headers: Headers::new(),
            config: serde_json::json!({
                "required_request_headers": "x-correlation-id,accept",
            }),
            ..RequestContext::default()
        };
        ctx.headers.insert("x-CORRELATION-ID", "abc");
        // `accept` missing → reject naming `accept` (the first missing).
        let d = g.guard_request(&ctx).await;
        match d {
            GuardDecision::Reject(r) => {
                assert_eq!(r.phase, GuardPhase::Request);
                assert_eq!(r.status, 400);
                assert_eq!(r.code, "REQUIRED_HEADER_MISSING");
                assert_eq!(r.detail, "accept");
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn response_phase_rejects_missing_header_502() {
        let g = guard();
        let ctx = ResponseContext {
            status: 200,
            headers: Headers::new(),
            config: serde_json::json!({ "required_response_headers": "content-type" }),
        };
        let d = g.guard_response(&ctx).await;
        match d {
            GuardDecision::Reject(r) => {
                assert_eq!(r.phase, GuardPhase::Response);
                assert_eq!(r.status, 502);
                assert_eq!(r.code, "REQUIRED_HEADER_MISSING");
                assert_eq!(r.detail, "content-type");
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn header_presence_is_case_insensitive() {
        let g = guard();
        let mut ctx = RequestContext {
            headers: Headers::new(),
            config: serde_json::json!({ "required_request_headers": "X-API-Version" }),
            ..RequestContext::default()
        };
        ctx.headers.insert("x-api-version", "2026-01");
        let d = g.guard_request(&ctx).await;
        assert!(d.allow());
    }

    #[tokio::test]
    async fn id_and_plugin_type_reflect_registration() {
        let g = guard();
        assert_eq!(g.id(), "required_headers");
        assert_eq!(g.plugin_type(), REQUIRED_HEADERS_GUARD);
    }
}
