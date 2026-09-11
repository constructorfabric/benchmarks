//! Shared API context and the domain-error → problem mapping.

use std::sync::Arc;

use crate::domain::error::{DomainError, Problem};

use crate::domain::service::ControlPlaneService;

/// Everything a REST handler needs, layered onto the router as an extension.
pub struct ApiContext {
    /// Control plane the handlers read and write through.
    pub control_plane: Arc<ControlPlaneService>,
    /// Effective gear configuration.
    pub config: Arc<crate::config::OagwConfig>,
    /// Outbound data plane used by the proxy handlers.
    pub proxy: Arc<crate::infra::proxy::service::ProxyService>,
    /// Tenant resolver used to walk the hierarchy at proxy time.
    pub tenants: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
}

/// Map a domain error onto a problem document, filling `instance`.
#[must_use]
pub fn instance_of(err: &DomainError) -> Problem {
    let mut problem = Problem::from_domain_error(err, None);
    problem.extensions.error_code = err.code();
    if matches!(
        err,
        DomainError::DownstreamError(_)
            | DomainError::ProtocolError(_)
            | DomainError::LinkUnavailable(_)
    ) {
        problem.extensions.host = Some(ApiContext::proxy_hostname());
    }
    problem
}

/// Build the response body of a gateway-generated proxy error.
#[must_use]
pub fn proxy_problem(err: &DomainError, path: &str) -> axum::response::Response {
    let mut problem = instance_of(err);
    problem.extensions.path = Some(path.to_owned());
    let mut response = problem.into_gateway_response();
    append_rate_limit_headers(&mut response, err);
    response
}

/// Add the `X-RateLimit-*` headers a 429 carries, per ADR 0003.
fn append_rate_limit_headers(response: &mut axum::response::Response, err: &DomainError) {
    let crate::domain::error::DomainError::RateLimitExceeded {
        limit,
        remaining,
        reset_secs,
        ..
    } = err
    else {
        return;
    };
    let headers = response.headers_mut();
    for (name, value) in [
        (crate::infra::proxy::headers::RATE_LIMIT, limit.to_string()),
        (
            crate::infra::proxy::headers::RATE_REMAINING,
            remaining.to_string(),
        ),
        (
            crate::infra::proxy::headers::RATE_RESET,
            reset_secs.to_string(),
        ),
    ] {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
}

impl ApiContext {
    /// The host string carried by upstream-facing error extensions.
    #[must_use]
    pub fn proxy_hostname() -> String {
        "gateway".to_owned()
    }
}

/// Extract the UUID from a GTS instance identifier, tolerating a bare UUID.
#[must_use]
pub fn uuid_from(value: &str) -> Option<uuid::Uuid> {
    crate::api::gts_id::parse_plugin_id(value).ok().or_else(|| {
        let tail = value.split('~').next_back()?;
        uuid::Uuid::parse_str(tail).ok()
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::model::gts;

    #[test]
    fn uuid_from_accepts_the_plugin_family() {
        let uuid = uuid::Uuid::new_v4();
        let value = format!("{}{uuid}", gts::GUARD_PLUGIN_TYPE);
        assert_eq!(uuid_from(&value), Some(uuid));
        assert_eq!(uuid_from("not-an-id"), None);
    }
}
