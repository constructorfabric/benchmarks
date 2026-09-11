//! The document an operator reads is the surface the gear actually serves.
//!
//! `register_rest` is called against a real registry here, and the resulting
//! `OpenAPI` document is held to the contract: every operation the gear registered
//! is recorded, every documented management path answers, and each data-plane
//! verb the gateway forwards is registered on its own.
//!
//! One caveat the tests keep honest about: the toolkit's document builder maps
//! `HEAD` and `OPTIONS` onto `GET` (it has no `HttpMethod` arm for them), so
//! those two verbs are asserted against the registry's operation list rather
//! than against the rendered document.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use serde_json::json;
use toolkit::Gear;
use toolkit::RestApiCapability;
use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};

/// The verbs a path item describes.
fn verbs_on(item: &utoipa::openapi::path::PathItem) -> Vec<String> {
    let mut out = Vec::new();
    for (verb, operation) in [
        ("get", &item.get),
        ("post", &item.post),
        ("put", &item.put),
        ("patch", &item.patch),
        ("delete", &item.delete),
        ("head", &item.head),
        ("options", &item.options),
        ("trace", &item.trace),
    ] {
        if operation.is_some() {
            out.push(verb.to_owned());
        }
    }
    out
}

/// The (method, path) pairs the document describes.
fn operations(document: &utoipa::openapi::OpenApi) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, item) in &document.paths.paths {
        for verb in verbs_on(item) {
            out.push((verb, path.clone()));
        }
    }
    out.sort();
    out
}

/// Mount the gear and return the document its registration renders.
fn mounted(
    hub: std::sync::Arc<toolkit::client_hub::ClientHub>,
) -> toolkit::api::OpenApiRegistryImpl {
    let ctx = toolkit::GearCtx::new(
        "oagw",
        uuid::Uuid::new_v4(),
        std::sync::Arc::new(common::TestConfigProvider {
            config: json!({"oagw": {"config": {"allow_http_upstream": true}}}),
        }),
        hub,
        tokio_util::sync::CancellationToken::new(),
    );
    let gear = oagw::gear::Oagw::default();
    tokio::runtime::Runtime::new()
        .expect("a runtime for init")
        .block_on(async { gear.init(&ctx).await })
        .expect("oagw initializes");

    let openapi = OpenApiRegistryImpl::new();
    let router = gear
        .register_rest(&ctx, axum::Router::new(), &openapi)
        .expect("oagw registers its routes");
    std::mem::forget(router);
    openapi
}

/// The gear's registration, both as recorded and as rendered.
fn registered() -> (Vec<(String, String)>, utoipa::openapi::OpenApi) {
    let hub = std::sync::Arc::new(toolkit::client_hub::ClientHub::new());
    hub.register::<dyn tenant_resolver_sdk::TenantResolverClient>(std::sync::Arc::new(
        common::FakeTenantResolver::rooted(uuid::Uuid::new_v4()),
    ));
    let recorded = mounted(hub.clone());
    let mut pairs: Vec<(String, String)> = recorded
        .operation_specs
        .iter()
        .map(|entry| (entry.value().method.to_string(), entry.value().path.clone()))
        .collect();
    pairs.sort();

    let rendered = mounted(hub);
    let document = rendered
        .build_openapi(&OpenApiInfo {
            title: "oagw".to_owned(),
            version: "v1".to_owned(),
            description: None,
            servers: vec![],
        })
        .expect("a document builds");
    (pairs, document)
}

/// The proxy paths the gear serves.
const PROXY_PATHS: [&str; 2] = ["/oagw/v1/proxy/{alias}", "/oagw/v1/proxy/{alias}/{*path}"];

/// The same paths as the rendered document spells them: the toolkit converts
/// axum's `{*path}` wildcard into `OpenAPI`'s `{path}`.
const PROXY_DOCUMENT_PATHS: [&str; 2] = ["/oagw/v1/proxy/{alias}", "/oagw/v1/proxy/{alias}/{path}"];

/// The management paths the gear serves.
const MANAGEMENT_PATHS: [&str; 6] = [
    "/oagw/v1/upstreams",
    "/oagw/v1/upstreams/{id}",
    "/oagw/v1/routes",
    "/oagw/v1/routes/{id}",
    "/oagw/v1/plugins",
    "/oagw/v1/plugins/{id}",
];

#[test]
fn every_forwarded_verb_is_registered_on_the_proxy_paths() {
    let (pairs, _) = registered();
    for path in PROXY_PATHS {
        let methods: Vec<&str> = pairs
            .iter()
            .filter(|(_, p)| p == path)
            .map(|(m, _)| m.as_str())
            .collect();
        for verb in ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"] {
            assert!(
                methods.contains(&verb),
                "{verb} is registered on {path}: {methods:?}"
            );
        }
        assert_eq!(
            methods.len(),
            7,
            "the data plane registers only these: {methods:?}"
        );
    }
}

#[test]
fn the_management_surface_is_documented() {
    let (pairs, document) = registered();
    for path in MANAGEMENT_PATHS {
        assert!(pairs.iter().any(|(_, p)| p == path), "{path} is registered");
        assert!(
            document.paths.paths.contains_key(path),
            "{path} is documented"
        );
    }
    assert!(
        pairs
            .iter()
            .any(|(m, p)| m == "GET" && p == "/oagw/v1/upstreams")
    );
    assert!(
        pairs
            .iter()
            .any(|(m, p)| m == "POST" && p == "/oagw/v1/upstreams")
    );
    assert!(
        pairs
            .iter()
            .any(|(m, p)| m == "DELETE" && p == "/oagw/v1/plugins/{id}")
    );
    assert!(
        pairs
            .iter()
            .any(|(m, p)| m == "DELETE" && p == "/oagw/v1/routes/{id}")
    );
}

#[test]
fn every_registered_operation_is_described_in_the_document() {
    let (pairs, document) = registered();
    // The document carries one operation per (method, path) it can express;
    // `HEAD` and `OPTIONS` collapse onto `GET` in the toolkit's builder, so
    // four of the proxy registrations are described by the `GET` they share a
    // path item with.
    let collapsible = 2 * PROXY_PATHS.len();
    let expected = pairs.len() - collapsible;
    let described = operations(&document);
    assert_eq!(
        described.len(),
        expected,
        "the document describes every operation the toolkit can express"
    );
    assert!(
        !described.is_empty(),
        "the gear describes a non-empty surface"
    );
}

#[test]
fn the_documented_proxy_paths_carry_the_forwarding_contracts() {
    let (_, document) = registered();
    for path in PROXY_DOCUMENT_PATHS {
        let item = document
            .paths
            .paths
            .get(path)
            .unwrap_or_else(|| panic!("{path} is documented"));
        let verbs = verbs_on(item);
        for verb in ["get", "post", "put", "patch", "delete"] {
            assert!(
                verbs.iter().any(|v| v.eq_ignore_ascii_case(verb)),
                "{verb} is documented on {path}: {verbs:?}"
            );
        }
    }
}
