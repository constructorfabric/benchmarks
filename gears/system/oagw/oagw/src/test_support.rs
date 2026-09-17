// Created: 2026-09-04 by Constructor Tech
//! Builders shared by the in-crate test modules.
//!
//! Kept minimal on purpose: they only assemble valid Phase-1 domain objects
//! with the few knobs the control-plane tests vary.

use uuid::Uuid;

use crate::domain::{
    Alias, Endpoint, EndpointScheme, HttpMatch, HttpMethod, PathSuffixMode, Protocol, Route,
    RouteMatch, RouteSpec, ServerConfig, Upstream, UpstreamSpec,
};

/// Default tenant of the test fixtures.
#[must_use]
pub fn tenant() -> Uuid {
    Uuid::new_v4()
}

/// A valid upstream spec for `host`, optionally requesting `alias`.
///
/// # Panics
///
/// Panics only on a bug in the fixture itself (the inputs are valid by
/// construction).
#[must_use]
pub fn upstream_spec(tenant_id: Uuid, host: &str, alias: Option<&str>) -> UpstreamSpec {
    UpstreamSpec {
        tenant_id,
        alias: alias
            .map(Alias::parse)
            .map(|parsed| parsed.expect("test alias is valid")),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig::new(vec![
            Endpoint::new(EndpointScheme::Https, host, None).expect("test host is valid"),
        ])
        .expect("a single endpoint is a valid pool"),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

/// A registered upstream with a server-generated id.
#[must_use]
pub fn upstream(spec: &UpstreamSpec) -> Upstream {
    Upstream::new(Uuid::new_v4(), spec).expect("test spec is valid")
}

/// An HTTP route of `upstream_id` matching `path` for `methods`.
#[must_use]
pub fn http_route(tenant_id: Uuid, upstream_id: Uuid, path: &str, methods: &[&str]) -> Route {
    let methods = methods
        .iter()
        .map(|method| HttpMethod::parse(method).expect("test method is valid"))
        .collect();
    let spec = RouteSpec {
        tenant_id,
        upstream_id,
        r#match: RouteMatch::Http(
            HttpMatch::new(methods, path.to_owned(), Vec::new(), PathSuffixMode::Append)
                .expect("test match is valid"),
        ),
        plugins: None,
        rate_limit: None,
        cors: None,
        enabled: true,
        tags: Vec::new(),
    };
    Route::new(Uuid::new_v4(), &spec).expect("test route is valid")
}
