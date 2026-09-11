//! Tests for the in-memory control plane: tenant scoping, alias uniqueness and
//! alias immutability.

use uuid::Uuid;

use super::{ControlPlane, REASON_ALIAS_CONFLICT, UpstreamRecord};
use crate::domain::model::{
    Endpoint, EndpointScheme, ServerConfig, UpstreamProtocol, UpstreamSpec, upstream_gts_id,
};

/// Tenant used by the "owner" side of the scoping tests.
fn tenant_a() -> Uuid {
    Uuid::from_u128(0xA)
}

/// Tenant used by the "other" side of the scoping tests.
fn tenant_b() -> Uuid {
    Uuid::from_u128(0xB)
}

fn spec_for(alias: Option<&str>, host: &str) -> UpstreamSpec {
    UpstreamSpec {
        id: None,
        alias: alias.map(str::to_owned),
        tags: Vec::new(),
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: host.to_owned(),
                port: 443,
            }],
        },
        protocol: UpstreamProtocol::Http,
        enabled: true,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

/// Same builder, used for IP-addressed endpoints (which need an explicit alias).
fn ip_spec(alias: Option<&str>, host: &str) -> UpstreamSpec {
    spec_for(alias, host)
}

#[test]
fn create_derives_the_alias_from_a_hostname_endpoint() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), spec_for(None, "Api.OpenAI.Com."))
        .expect("create must succeed");

    assert_eq!(record.alias, "api.openai.com");
    assert_eq!(record.tenant_id, tenant_a());

    let wire = record.wire();
    assert_eq!(
        wire.id.as_deref(),
        Some(upstream_gts_id(record.id).as_str())
    );
    assert_eq!(wire.alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn create_requires_an_alias_for_ip_endpoints() {
    let plane = ControlPlane::new();
    let error = plane
        .create_upstream(tenant_a(), ip_spec(None, "10.0.1.1"))
        .expect_err("IP endpoints need an explicit alias");
    assert_eq!(error.status_code(), 400);

    let record = plane
        .create_upstream(tenant_a(), ip_spec(Some("My-Service"), "10.0.1.1"))
        .expect("explicit alias must be accepted");
    assert_eq!(record.alias, "my-service");
}

#[test]
fn create_rejects_an_alias_that_disagrees_with_the_derivation() {
    let plane = ControlPlane::new();
    let error = plane
        .create_upstream(tenant_a(), spec_for(Some("my-openai"), "api.openai.com"))
        .expect_err("alias override must be rejected");
    assert_eq!(error.status_code(), 400);
    assert_eq!(
        error.extensions().invalid_value.as_deref(),
        Some("my-openai")
    );
}

#[test]
fn create_rejects_invalid_documents() {
    let plane = ControlPlane::new();
    let mut spec = spec_for(None, "api.openai.com");
    spec.server.endpoints.clear();

    let error = plane
        .create_upstream(tenant_a(), spec)
        .expect_err("empty endpoint set must be rejected");
    assert_eq!(error.status_code(), 400);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[test]
fn alias_is_unique_per_tenant() {
    let plane = ControlPlane::new();
    plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("first create");

    let error = plane
        .create_upstream(tenant_a(), spec_for(None, "API.OpenAI.COM."))
        .expect_err("duplicate alias must conflict");
    assert_eq!(error.status_code(), 409);
    assert_eq!(
        error.extensions().reason.as_deref(),
        Some(REASON_ALIAS_CONFLICT)
    );
    assert_eq!(error.extensions().alias.as_deref(), Some("api.openai.com"));

    // A different tenant may use the same alias (no global uniqueness).
    plane
        .create_upstream(tenant_b(), spec_for(None, "api.openai.com"))
        .expect("another tenant may reuse the alias");
}

#[test]
fn reads_are_tenant_scoped() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("create");

    assert_eq!(
        plane
            .get_upstream(tenant_a(), record.id)
            .expect("owner reads its upstream")
            .id,
        record.id
    );
    let error = plane
        .get_upstream(tenant_b(), record.id)
        .expect_err("another tenant must not see the upstream");
    assert_eq!(error.status_code(), 404);
    assert_eq!(
        error.extensions().upstream_id.as_deref(),
        Some(upstream_gts_id(record.id).as_str())
    );
}

#[test]
fn list_is_ordered_paginated_and_tenant_scoped() {
    let plane = ControlPlane::new();
    plane
        .create_upstream(tenant_a(), spec_for(None, "zebra.example.com"))
        .expect("create zebra");
    plane
        .create_upstream(tenant_a(), spec_for(None, "alpha.example.com"))
        .expect("create alpha");
    plane
        .create_upstream(tenant_b(), spec_for(None, "other.example.com"))
        .expect("create other");

    let all = plane.list_upstreams(tenant_a(), 50, 0);
    let aliases: Vec<&str> = all.iter().map(|record| record.alias.as_str()).collect();
    assert_eq!(aliases, ["alpha.example.com", "zebra.example.com"]);

    let page = plane.list_upstreams(tenant_a(), 1, 1);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].alias, "zebra.example.com");

    assert_eq!(plane.list_upstreams(tenant_b(), 50, 0).len(), 1);
    assert!(plane.list_upstreams(tenant_a(), 100, 5).is_empty());
}

#[test]
fn update_replaces_the_document_and_keeps_the_alias() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("create");

    let mut updated = spec_for(None, "api.openai.com");
    updated.enabled = false;
    updated.tags = vec!["llm".to_owned()];
    let stored = plane
        .update_upstream(tenant_a(), record.id, updated)
        .expect("update must succeed");

    assert_eq!(stored.id, record.id);
    assert_eq!(stored.alias, "api.openai.com");
    assert!(!stored.spec.enabled);
    assert_eq!(stored.spec.tags, ["llm"]);
    assert_eq!(
        plane
            .get_upstream(tenant_a(), record.id)
            .expect("read back")
            .spec
            .tags,
        ["llm"]
    );
}

#[test]
fn update_is_tenant_scoped_and_rejects_alias_changes() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("create");

    let error = plane
        .update_upstream(tenant_b(), record.id, spec_for(None, "api.openai.com"))
        .expect_err("another tenant must not update the upstream");
    assert_eq!(error.status_code(), 404);

    let error = plane
        .update_upstream(
            tenant_a(),
            record.id,
            spec_for(Some("other"), "api.openai.com"),
        )
        .expect_err("alias override must be rejected");
    assert_eq!(error.status_code(), 400);

    // Moving to an IP endpoint would drop the derivation → rejected.
    let error = plane
        .update_upstream(
            tenant_a(),
            record.id,
            ip_spec(Some("api.openai.com"), "10.0.1.1"),
        )
        .expect_err("hostname to IP must be rejected");
    assert_eq!(error.status_code(), 400);
}

#[test]
fn update_allows_ip_upstreams_to_move_hosts() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), ip_spec(Some("my-service"), "10.0.1.1"))
        .expect("create");

    let moved = plane
        .update_upstream(tenant_a(), record.id, ip_spec(None, "10.0.1.9"))
        .expect("IP to IP keeps the alias");
    assert_eq!(moved.alias, "my-service");
    assert_eq!(moved.spec.server.endpoints[0].host, "10.0.1.9");
}

#[test]
fn delete_removes_the_record_and_frees_the_alias() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("create");

    let removed = plane
        .delete_upstream(tenant_a(), record.id)
        .expect("delete must succeed");
    assert_eq!(removed.id, record.id);
    let error = plane
        .get_upstream(tenant_a(), record.id)
        .expect_err("the record is gone");
    assert_eq!(error.status_code(), 404);

    plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("the alias is free again");
}

#[test]
fn delete_is_tenant_scoped() {
    let plane = ControlPlane::new();
    let record = plane
        .create_upstream(tenant_a(), spec_for(None, "api.openai.com"))
        .expect("create");

    let error = plane
        .delete_upstream(tenant_b(), record.id)
        .expect_err("another tenant must not delete the upstream");
    assert_eq!(error.status_code(), 404);
    assert_eq!(
        plane
            .get_upstream(tenant_a(), record.id)
            .expect("still there")
            .id,
        record.id
    );
}

#[test]
fn records_expose_their_wire_representation() {
    let record = UpstreamRecord {
        id: Uuid::from_u128(0x1),
        tenant_id: tenant_a(),
        alias: "api.openai.com".to_owned(),
        spec: spec_for(None, "api.openai.com"),
    };
    let wire = record.wire();
    assert_eq!(
        wire.id.as_deref(),
        Some(upstream_gts_id(record.id).as_str())
    );
    assert_eq!(wire.alias.as_deref(), Some("api.openai.com"));
    assert_eq!(record.spec.id, None, "the stored document carries no id");
}
