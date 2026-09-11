//! The route management surface: `POST/GET/PUT/DELETE /oagw/v1/routes`.
//!
//! A route is the only thing that turns an endpoint pool into traffic, so the
//! tests pin the validation the control plane performs on the way in — path,
//! methods, target alias resolution, CORS and plugin bindings — and the
//! lifecycle an operator sees afterwards.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{LocalUpstream, app};
use serde_json::json;

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

#[tokio::test]
async fn a_route_is_created_against_a_resolved_alias() {
    let app = app().await;
    let (upstream, route) = app
        .wire(
            json!({
                "alias": "target.partner.com",
                "endpoints": [{"scheme": "https", "host": "target.partner.com"}]
            }),
            "/v1/payments",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    assert_eq!(route["path"], "/v1/payments");
    assert_eq!(route["target_alias"], alias);
    // The data model defaults `strip_prefix` on: a route forwards the
    // remainder of the path unless the operator says otherwise.
    assert_eq!(route["strip_prefix"], true);
    assert_eq!(route["preserve_host"], false);
    assert_eq!(route["enabled"], true);
    assert_eq!(route["priority"], 0);
    assert_eq!(route["methods"], json!(METHODS));
    assert!(route["id"].as_str().is_some(), "{route}");
}

/// A route cannot be created against an alias that does not resolve in scope.
#[tokio::test]
async fn an_unknown_target_alias_is_rejected() {
    let app = app().await;
    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/nowhere",
            "methods": ["GET"],
            "target_alias": "ghost.partner.com"
        }))
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn a_route_path_must_start_with_a_slash() {
    let app = app().await;
    let (upstream, _) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "v.partner.com"}]}),
            "/v1/anchor",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    let (status, document) = app
        .create_route_bad(json!({
            "path": "v1/relative",
            "methods": ["GET"],
            "target_alias": alias
        }))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert_eq!(document["title"], "Validation failed");
}

#[tokio::test]
async fn a_route_must_allow_at_least_one_method() {
    let app = app().await;
    let (upstream, _) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "m.partner.com"}]}),
            "/v1/anchor",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    let (status, _) = app
        .create_route_bad(json!({
            "path": "/v1/quiet",
            "methods": [],
            "target_alias": alias
        }))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

/// A method token is checked for syntax, not for being a known verb: `FETCH`
/// would be a legal extension method, but a token with a space is not a token.
#[tokio::test]
async fn a_method_that_is_not_an_http_token_is_rejected() {
    let app = app().await;
    let (upstream, _) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "t.partner.com"}]}),
            "/v1/anchor",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    let (status, _) = app
        .create_route_bad(json!({
            "path": "/v1/odd",
            "methods": ["NOT A METHOD"],
            "target_alias": alias
        }))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

/// `*` stands in for every method, so a route may accept the lot.
#[tokio::test]
async fn a_wildcard_method_is_accepted() {
    let app = app().await;
    let upstream = app
        .create_upstream(json!({
            "alias": "star.partner.com",
            "endpoints": [{"scheme": "https", "host": "star.partner.com"}]
        }))
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    let (status, route) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/routes",
            Some(json!({
                "path": "/v1/star",
                "methods": ["*"],
                "target_alias": alias
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);
    assert_eq!(route["methods"], json!(["*"]));
}

#[tokio::test]
async fn a_route_is_readable_replaced_and_deleted() {
    let app = app().await;
    let (upstream, route) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "crud.partner.com"}]}),
            "/v1/first",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();
    let id = route["id"].as_str().unwrap().to_owned();

    let (status, read) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/routes/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(read["path"], "/v1/first");

    let (status, replaced) = app
        .send_json(
            http::Method::PUT,
            &format!("/oagw/v1/routes/{id}"),
            Some(json!({
                "path": "/v1/second",
                "methods": ["GET"],
                "target_alias": alias,
                "strip_prefix": true
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(replaced["path"], "/v1/second");
    assert_eq!(replaced["strip_prefix"], true);
    assert_eq!(replaced["id"], json!(id), "the identity survives a replace");

    let response = app
        .send(app.request(
            http::Method::DELETE,
            &format!("/oagw/v1/routes/{id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/routes/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_route_is_not_found() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/routes/{}", uuid::Uuid::new_v4()),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn a_route_list_reports_the_total_across_pages() {
    let app = app().await;
    let upstream = app
        .create_upstream(json!({
            "alias": "paging.partner.com",
            "endpoints": [{"scheme": "https", "host": "paging.partner.com"}]
        }))
        .await;
    let alias = upstream["alias"].as_str().unwrap();
    for index in 0..3 {
        app.create_route(json!({
            "path": format!("/v1/page{index}"),
            "methods": ["GET"],
            "target_alias": alias
        }))
        .await;
    }

    let (status, page) = app
        .send_json(http::Method::GET, "/oagw/v1/routes?$top=2", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(page["total_count"], 3);
}

/// A route's CORS block is validated on the way in, not at request time: the
/// one forbidden combination is credentials alongside a wildcard origin.
#[tokio::test]
async fn credentials_with_a_wildcard_origin_are_rejected() {
    let app = app().await;
    let (upstream, _) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "cors.partner.com"}]}),
            "/v1/anchor",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/cors",
            "methods": ["GET"],
            "target_alias": alias,
            "cors": {
                "allow_origins": ["*"],
                "allow_methods": ["*"],
                "allow_credentials": true
            }
        }))
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
}

#[tokio::test]
async fn an_unknown_plugin_is_rejected_at_bind_time() {
    let app = app().await;
    let (upstream, _) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "plug.partner.com"}]}),
            "/v1/anchor",
        )
        .await;
    let alias = upstream["alias"].as_str().unwrap();

    let (status, document) = app
        .create_route_bad(json!({
            "path": "/v1/plugged",
            "methods": ["GET"],
            "target_alias": alias,
            "plugins": [{"plugin_id": "no-such-plugin", "config": {}}]
        }))
        .await;
    // An unresolvable binding is reported against the catalog, which the
    // gateway surfaces as a service-unavailable problem document.
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        document["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
}

/// A deleted route frees the upstream it referenced.
#[tokio::test]
async fn deleting_a_route_releases_its_upstream() {
    let app = app().await;
    let (upstream, route) = app
        .wire(
            json!({"endpoints": [{"scheme": "https", "host": "release.partner.com"}]}),
            "/v1/hold",
        )
        .await;
    let id = route["id"].as_str().unwrap().to_owned();
    app.send(app.request(
        http::Method::DELETE,
        &format!("/oagw/v1/routes/{id}"),
        None,
        &[],
    ))
    .await;

    let upstream_id = upstream["id"].as_str().unwrap().to_owned();
    let response = app
        .send(app.request(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        http::StatusCode::NO_CONTENT,
        "an unreferenced upstream deletes cleanly again"
    );
}

/// `path_suffix_mode` round-trips with the schema's default `append`.
#[tokio::test]
async fn path_suffix_mode_defaults_to_append_and_round_trips() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let default_alias = app
        .create_upstream(upstream.upstream_spec("suffix.default"))
        .await["alias"]
        .as_str()
        .unwrap()
        .to_owned();
    let (defaulted, default_document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/routes",
            Some(json!({
                "path": "/v1/default",
                "methods": ["GET"],
                "target_alias": default_alias,
                "strip_prefix": false
            })),
            &[],
        )
        .await;
    assert_eq!(defaulted, http::StatusCode::CREATED, "{default_document}");
    assert_eq!(
        default_document["path_suffix_mode"], "append",
        "the schema's default"
    );

    let spec = upstream.upstream_spec("suffix.disabled");
    let upstream_doc = app.create_upstream(spec).await;
    let alias = upstream_doc["alias"].as_str().unwrap().to_owned();
    let (created, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/routes",
            Some(json!({
                "path": "/v1/fixed",
                "methods": ["GET"],
                "target_alias": alias,
                "strip_prefix": false,
                "path_suffix_mode": "disabled"
            })),
            &[],
        )
        .await;
    assert_eq!(created, http::StatusCode::CREATED, "{document}");
    let id = document["id"].as_str().unwrap().to_owned();
    assert_eq!(document["path_suffix_mode"], "disabled");

    let (_, read) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/routes/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(read["path_suffix_mode"], "disabled");
}

/// An unknown mode is refused rather than silently treated as one of the two.
#[tokio::test]
async fn an_unknown_path_suffix_mode_is_refused() {
    let app = app().await;
    let upstream = LocalUpstream::start().await;
    let spec = upstream.upstream_spec("suffix.bad");
    let upstream_doc = app.create_upstream(spec).await;
    let (status, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/routes",
            Some(json!({
                "path": "/v1/fixed",
                "methods": ["GET"],
                "target_alias": upstream_doc["alias"],
                "strip_prefix": false,
                "path_suffix_mode": "sometimes"
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{document}");
}
