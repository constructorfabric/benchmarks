//! The `required_headers` guard plugin (ADR 0009).
//!
//! The binding lists request headers that must be present. A missing one
//! rejects the exchange with a 400; a blank configuration fails **open**, so a
//! route that binds the guard without naming any header keeps working.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginType, RequestContext, ResponseContext,
};
use crate::infra::plugin::registry::PluginFactory;

/// The configuration key listing the required header names.
pub const HEADERS_KEY: &str = "headers";

/// The error code a rejection carries in its detail.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Rejects requests that lack a configured header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredHeadersGuard {
    headers: Vec<String>,
}

impl RequiredHeadersGuard {
    /// Build the guard from its binding configuration.
    ///
    /// A blank or missing `headers` list is a valid configuration that fails
    /// open.
    #[must_use]
    pub fn from_config(config: &serde_json::Map<String, serde_json::Value>) -> Self {
        let headers = config
            .get(HEADERS_KEY)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self { headers }
    }

    /// The headers this guard requires, as configured.
    #[must_use]
    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    /// Whether the configuration requires anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// The first configured header missing from `headers`, if any.
    #[must_use]
    pub fn missing(&self, headers: &http::HeaderMap) -> Option<String> {
        self.headers.iter().find_map(|name| {
            let key = http::HeaderName::from_bytes(name.as_bytes()).ok()?;
            if headers.contains_key(&key) {
                None
            } else {
                Some(name.clone())
            }
        })
    }

    fn rejection(name: &str) -> DomainError {
        DomainError::validation(format!("{REQUIRED_HEADER_MISSING}: {name}"))
    }
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuard {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    fn plugin_type(&self) -> &'static str {
        "guard"
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, DomainError> {
        if self.is_empty() {
            return Ok(GuardDecision::Allow);
        }
        match self.missing(&ctx.headers) {
            Some(name) => Ok(GuardDecision::Reject(Self::rejection(&name))),
            None => Ok(GuardDecision::Allow),
        }
    }

    async fn guard_response(&self, _ctx: &ResponseContext) -> Result<GuardDecision, DomainError> {
        Ok(GuardDecision::Allow)
    }
}

/// Builds `required_headers` instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardFactory;

impl PluginFactory<dyn GuardPlugin> for RequiredHeadersGuardFactory {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Guard
    }

    fn description(&self) -> &'static str {
        "Rejects a request that lacks any of the configured headers"
    }

    fn create(
        &self,
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn GuardPlugin>, DomainError> {
        Ok(Arc::new(RequiredHeadersGuard::from_config(config)))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod required_headers_guard_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;
    use serde_json::json;

    fn config(raw: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        raw.as_object().expect("object").clone()
    }

    fn context(headers: &[(&str, &str)]) -> RequestContext {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        RequestContext {
            method: "GET".to_owned(),
            path: "/v1".to_owned(),
            query: String::new(),
            headers: map,
            body_present: false,
            security_context: toolkit_security::SecurityContext::anonymous(),
            tenant_scope: vec![uuid::Uuid::nil()],
            injected_headers: Vec::new(),
            attributes: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn rejects_a_request_missing_the_header() {
        let guard =
            RequiredHeadersGuard::from_config(&config(&json!({"headers": ["X-Tenant-Id"]})));
        let decision = guard.guard_request(&context(&[])).await.unwrap();
        let rejection = decision.rejection().expect("rejected");
        assert_eq!(rejection.kind(), ErrorKind::Validation);
        assert_eq!(
            rejection.http_status(),
            http::StatusCode::BAD_REQUEST,
            "a missing header is a 400"
        );
        assert!(rejection.detail().contains(REQUIRED_HEADER_MISSING));
    }

    #[tokio::test]
    async fn header_names_are_matched_case_insensitively() {
        let guard =
            RequiredHeadersGuard::from_config(&config(&json!({"headers": ["x-tenant-id"]})));
        let decision = guard
            .guard_request(&context(&[("X-TENANT-ID", "t1")]))
            .await
            .unwrap();
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn a_present_header_is_accepted() {
        let guard = RequiredHeadersGuard::from_config(&config(&json!({"headers": ["A", "B"]})));
        assert!(
            guard
                .guard_request(&context(&[("a", "1"), ("B", "2")]))
                .await
                .unwrap()
                .is_allowed()
        );
    }

    #[tokio::test]
    async fn a_blank_configuration_fails_open() {
        let blanks = [
            json!({}),
            json!({"headers": []}),
            json!({"headers": [" ", ""]}),
        ];
        for raw in &blanks {
            let guard = RequiredHeadersGuard::from_config(&config(raw));
            assert!(guard.is_empty(), "{guard:?} must require nothing");
            let decision = guard.guard_request(&context(&[])).await.unwrap();
            assert!(
                decision.is_allowed(),
                "ADR 0009: blank configuration fails open"
            );
        }
    }

    #[tokio::test]
    async fn responses_are_always_allowed() {
        let guard = RequiredHeadersGuard::from_config(&config(&json!({"headers": ["X-Anything"]})));
        let response = ResponseContext {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            injected_headers: Vec::new(),
        };
        assert!(guard.guard_response(&response).await.unwrap().is_allowed());
    }
}
