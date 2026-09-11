//! Fixtures for the plugin unit tests.
//!
//! Kept out of the lib's public surface: the module is compiled only under
//! `cfg(test)`.

use std::sync::Arc;

use http::HeaderMap;

use crate::domain::plugin::{RequestContext, ResponseContext};

/// Tenant every fixture runs under.
#[must_use]
pub fn tenant_id() -> uuid::Uuid {
    uuid::Uuid::nil()
}

/// A security context for [`tenant_id`].
#[must_use]
pub fn security() -> Arc<toolkit_security::SecurityContext> {
    Arc::new(
        toolkit_security::SecurityContext::builder()
            .subject_tenant_id(tenant_id())
            .subject_id(uuid::Uuid::nil())
            .build()
            .expect("security context builds"),
    )
}

/// An empty request context addressed to `alias`.
#[must_use]
pub fn request_context(alias: &str) -> RequestContext {
    RequestContext {
        headers: HeaderMap::new(),
        path: "/echo".to_owned(),
        alias: alias.to_owned(),
        tenant_id: tenant_id(),
        security: security(),
        config: serde_json::Value::Null,
        trace: Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

/// An empty response context addressed to `alias`.
#[must_use]
pub fn response_context(alias: &str) -> ResponseContext {
    ResponseContext {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        alias: alias.to_owned(),
        tenant_id: tenant_id(),
        config: serde_json::Value::Null,
        trace: Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

/// The plugin phases a fixture observed.
#[must_use]
pub fn recorded(ctx: &RequestContext) -> Vec<String> {
    ctx.trace
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fixtures_are_isolated_per_context() {
        let first = request_context("a");
        let second = request_context("a");
        first.record("noop", crate::domain::plugin::PluginPhase::Request);
        assert_eq!(recorded(&first), vec!["noop:Request".to_owned()]);
        assert!(recorded(&second).is_empty());
    }
}
