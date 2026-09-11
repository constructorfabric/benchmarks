//! The plugin catalog is what an operator is allowed to bind.
//!
//! A built-in is implemented here; a catalog-only entry is a name the gear
//! recognises and refuses. These tests hold the catalog's surface to its
//! contract — list, read, and a delete that will not take a bound plugin away
//! from a route that is using it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use http_body_util::BodyExt;
use serde_json::json;

/// The plugins this build implements.
const BUILT_INS: &[&str] = &["noop", "apikey", "required_headers", "request_id"];

/// The identifiers the catalog knows but this build does not implement.
const CATALOG_ONLY: &[&str] = &[
    "basic", "bearer", "oauth2", "timeout", "cors", "logging", "metrics",
];

#[tokio::test]
async fn the_catalog_lists_built_ins_and_catalog_only_entries() {
    let app = app().await;
    let (status, document) = app
        .send_json(http::Method::GET, "/oagw/v1/plugins", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK, "{document}");

    let items = document["items"].as_array().expect("a list of plugins");
    let ids: Vec<&str> = items
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    for expected in BUILT_INS {
        assert!(ids.contains(expected), "{expected} is a built-in: {ids:?}");
    }
    for expected in CATALOG_ONLY {
        assert!(
            ids.contains(expected),
            "{expected} is catalog-only: {ids:?}"
        );
    }
    assert_eq!(
        document["total_count"].as_u64(),
        Some(u64::try_from(items.len()).unwrap_or(u64::MAX)),
        "the page counts itself"
    );

    for entry in items {
        assert!(
            entry["plugin_type"].is_string(),
            "each entry names its class: {entry}"
        );
        assert!(entry["version"].is_string(), "{entry}");
        assert!(entry["description"].is_string(), "{entry}");
        assert!(entry["built_in"].is_boolean(), "{entry}");
    }
    for entry in items {
        let id = entry["id"].as_str().unwrap();
        let built_in = BUILT_INS.contains(&id);
        assert_eq!(
            entry["built_in"].as_bool(),
            Some(built_in),
            "{id} is labelled as implemented only when it is"
        );
    }
}

#[tokio::test]
async fn a_single_catalog_entry_is_readable() {
    let app = app().await;
    let (status, document) = app
        .send_json(http::Method::GET, "/oagw/v1/plugins/request_id", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK, "{document}");
    assert_eq!(document["id"], "request_id");
    assert_eq!(document["built_in"], true);
}

#[tokio::test]
async fn an_unknown_plugin_is_not_found() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::GET,
            "/oagw/v1/plugins/not_a_plugin",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{document}");
}

/// A catalog-only identifier is a known name with nothing behind it, so a
/// route that binds it is refused rather than left unworkable.
#[tokio::test]
async fn binding_a_catalog_only_id_is_rejected() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("bound.to.nothing");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/thing",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "plugins": [{"plugin_id": "basic", "config": {}}]
        }))
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{document}");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("plugin.not_found.v1")),
        "{document}"
    );
}

/// A plugin a route still binds cannot be taken out of the catalog.
#[tokio::test]
async fn deleting_a_bound_plugin_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("keeps.a.plugin");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    app.create_route(json!({
        "path": "/v1/thing",
        "methods": ["GET"],
        "target_alias": alias,
        "strip_prefix": false,
        "plugins": [{"plugin_id": "request_id", "config": {}}]
    }))
    .await;

    let (status, document) = app
        .send_json(
            http::Method::DELETE,
            "/oagw/v1/plugins/request_id",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{document}");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("plugin.in_use.v1")),
        "{document}"
    );

    // The route still works: the catalog refused to lose the implementation.
    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/proxy/{alias}/v1/thing"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK);
}

#[tokio::test]
async fn deleting_an_unbound_plugin_is_allowed() {
    let app = app().await;
    // `noop` is a built-in no route below binds.
    let (status, document) = app
        .send_json(http::Method::DELETE, "/oagw/v1/plugins/noop", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT, "{document}");

    // A second delete finds nothing left to remove.
    let (status, document) = app
        .send_json(http::Method::DELETE, "/oagw/v1/plugins/noop", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{document}");
}

/// The catalog is not tenant data, but it is read through a permission of its
/// own: a token scoped away from it is refused.
#[tokio::test]
async fn the_catalog_refuses_a_subject_without_the_permission() {
    let app = app().await;
    let scoped = toolkit_security::SecurityContext::builder()
        .subject_id(uuid::Uuid::new_v4())
        .subject_tenant_id(uuid::Uuid::new_v4())
        .token_scopes(vec!["gts.cf.core.oagw.proxy.v1~:invoke".to_owned()])
        .build()
        .expect("valid security context");
    let response = app
        .send(common::request_with_subject(
            scoped,
            http::Method::GET,
            "/oagw/v1/plugins",
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::UNAUTHORIZED,
        "{response:?}"
    );
}

/// Retiring a plugin keeps it out of the catalog and unbindable.
#[tokio::test]
async fn a_retired_plugin_is_gone_from_the_catalog_and_unbindable() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("after.retirement");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();

    let (status, _) = app
        .send_json(http::Method::DELETE, "/oagw/v1/plugins/apikey", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);

    let (status, document) = app
        .send_json(http::Method::GET, "/oagw/v1/plugins", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK, "{document}");
    let ids: Vec<&str> = document["items"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"apikey"),
        "a retired plugin is not listed: {ids:?}"
    );

    let (status, _) = app
        .send_json(http::Method::GET, "/oagw/v1/plugins/apikey", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);

    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/thing",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "plugins": [{"plugin_id": "apikey", "config": {}}]
        }))
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{document}");
}

/// The source a tenant-defined plugin ships, and the description it is filed
/// under.
const SOURCE: &str = "def transform_request(ctx):\n    ctx.set_header('x-redacted', 'true')\n";
const DESCRIPTION: &str = "Redacts a response header";

/// File a tenant-defined plugin in the catalog.
async fn create_plugin(app: &common::TestApp, id: &str) -> (http::StatusCode, serde_json::Value) {
    app.send_json(
        http::Method::POST,
        "/oagw/v1/plugins",
        Some(json!({
            "id": id,
            "plugin_type": "transform",
            "source": SOURCE,
            "description": DESCRIPTION
        })),
        &[],
    )
    .await
}

#[tokio::test]
async fn a_tenant_defined_plugin_can_be_created_and_read() {
    let app = app().await;
    let (status, document) = create_plugin(&app, "tenant.redact.v1").await;
    assert_eq!(status, http::StatusCode::CREATED, "{document}");
    assert_eq!(document["id"], "tenant.redact.v1");
    assert_eq!(document["plugin_type"], "transform");
    assert_eq!(document["built_in"], false, "the gear did not ship it");
    assert_eq!(document["description"], DESCRIPTION);

    let (status, read) = app
        .send_json(
            http::Method::GET,
            "/oagw/v1/plugins/tenant.redact.v1",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{read}");
    assert_eq!(read["id"], "tenant.redact.v1");
    assert_eq!(read["built_in"], false);

    let (status, list) = app
        .send_json(http::Method::GET, "/oagw/v1/plugins", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK, "{list}");
    assert!(
        list["items"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|entry| entry["id"] == "tenant.redact.v1" && entry["built_in"] == false),
        "the stored plugin is listed: {list}"
    );
}

#[tokio::test]
async fn a_custom_plugins_source_is_served_verbatim() {
    let app = app().await;
    let (status, document) = create_plugin(&app, "tenant.source.v1").await;
    assert_eq!(status, http::StatusCode::CREATED, "{document}");

    let response = app
        .send(app.request(
            http::Method::GET,
            "/oagw/v1/plugins/tenant.source.v1/source",
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::OK, "{response:?}");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; charset=utf-8"),
        "{response:?}"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert_eq!(bytes.as_ref(), SOURCE.as_bytes(), "byte for byte");
}

/// A plugin definition is immutable once filed: a change is a new plugin.
#[tokio::test]
async fn a_custom_plugin_cannot_be_replaced() {
    let app = app().await;
    let (status, document) = create_plugin(&app, "tenant.immutable.v1").await;
    assert_eq!(status, http::StatusCode::CREATED, "{document}");

    let (status, refused) = app
        .send_json(
            http::Method::PUT,
            "/oagw/v1/plugins/tenant.immutable.v1",
            Some(json!({"source": "def transform_request(ctx): pass"})),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{refused}");
    let (status, read) = app
        .send_json(
            http::Method::GET,
            "/oagw/v1/plugins/tenant.immutable.v1",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{read}");
    assert_eq!(read["description"], DESCRIPTION, "nothing was overwritten");
}

/// A plugin a route binds cannot be taken out of the catalog until the route
/// lets go of it.
#[tokio::test]
async fn a_bound_custom_plugin_deletes_only_once_unbound() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("custom.bound");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let (status, document) = create_plugin(&app, "tenant.bound.v1").await;
    assert_eq!(status, http::StatusCode::CREATED, "{document}");
    let route = app
        .create_route(json!({
            "path": "/v1/thing",
            "methods": ["GET"],
            "target_alias": alias,
            "strip_prefix": false,
            "plugins": [{"plugin_id": "tenant.bound.v1", "config": {}}]
        }))
        .await;

    let (status, refused) = app
        .send_json(
            http::Method::DELETE,
            "/oagw/v1/plugins/tenant.bound.v1",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("plugin.in_use.v1")),
        "{refused}"
    );

    let route_id = route["id"].as_str().unwrap().to_owned();
    let (status, _) = app
        .send_json(
            http::Method::DELETE,
            &format!("/oagw/v1/routes/{route_id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);

    let (status, _) = app
        .send_json(
            http::Method::DELETE,
            "/oagw/v1/plugins/tenant.bound.v1",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
}

/// A custom plugin is filed per tenant: another tenant cannot see it, and a
/// duplicate identifier is refused rather than silently replaced.
#[tokio::test]
async fn a_custom_plugin_is_scoped_to_its_tenant() {
    let app = app().await;
    let (status, _) = create_plugin(&app, "tenant.scoped.v1").await;
    assert_eq!(status, http::StatusCode::CREATED);

    let (status, document) = create_plugin(&app, "tenant.scoped.v1").await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{document}");

    let (status, document) = app
        .send_json_as(
            app.foreign,
            http::Method::GET,
            "/oagw/v1/plugins/tenant.scoped.v1",
            None,
            &[],
        )
        .await;
    assert_eq!(
        status,
        http::StatusCode::NOT_FOUND,
        "another tenant reads none of it: {document}"
    );
}
