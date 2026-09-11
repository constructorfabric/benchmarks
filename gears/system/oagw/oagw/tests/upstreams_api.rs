//! The upstream management surface: `POST/GET/PATCH/DELETE /oagw/v1/upstreams`.
//!
//! Alias derivation, uniqueness, tenant scoping and the delete guard are the
//! behaviours operators lean on, so each is pinned here against the wire
//! contract rather than against the domain types.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::app;
use serde_json::json;

#[tokio::test]
async fn an_upstream_is_created_with_a_derived_alias() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "name": "Payments",
                "endpoints": [{"scheme": "https", "host": "api.partner.com", "port": 8443}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);
    // One host: the alias derives from it, and the non-default port travels with it.
    assert_eq!(document["alias"], "api.partner.com:8443");
    assert_eq!(document["name"], "Payments");
    assert_eq!(document["tenant_id"], json!(app.tenant));
    assert_eq!(document["load_balancing"], "round_robin");
}

#[tokio::test]
async fn an_explicit_alias_is_normalised_and_kept() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "API.Partner.COM.",
                "name": "Payments",
                "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);
    assert_eq!(document["alias"], "api.partner.com");
}

#[tokio::test]
async fn an_alias_may_not_be_reused() {
    let app = app().await;
    let body = json!({
        "alias": "taken.partner.com",
        "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
    });
    let (status, _) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(body.clone()),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);

    let (status, document) = app
        .send_json(http::Method::POST, "/oagw/v1/upstreams", Some(body), &[])
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "second use conflicts");
    assert_eq!(document["status"], 409, "{document}");
    assert!(
        document["type"]
            .as_str()
            .unwrap_or_default()
            .starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
        "the problem document carries a GTS type: {document}"
    );
}

#[tokio::test]
async fn an_empty_endpoint_pool_is_rejected() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({"name": "Empty", "endpoints": []})),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert!(
        !document["title"].as_str().unwrap_or_default().is_empty(),
        "a validation failure is a problem document: {document}"
    );
}

#[tokio::test]
async fn an_endpoint_without_a_host_is_rejected() {
    let app = app().await;
    let (status, _) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({"endpoints": [{"scheme": "https"}]})),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_bare_public_suffix_is_not_an_alias() {
    let app = app().await;
    let (status, _) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "endpoints": [
                    {"scheme": "https", "host": "a.co.uk"},
                    {"scheme": "https", "host": "b.co.uk"}
                ]
            })),
            &[],
        )
        .await;
    assert_eq!(
        status,
        http::StatusCode::BAD_REQUEST,
        "co.uk alone is not addressable"
    );
}

#[tokio::test]
async fn an_upstream_is_readable_by_id() {
    let app = app().await;
    let created = app
        .create_upstream(json!({
            "alias": "read.partner.com",
            "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
        }))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(document["alias"], "read.partner.com");
}

#[tokio::test]
async fn an_unknown_upstream_is_not_found() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{}", uuid::Uuid::new_v4()),
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
async fn a_foreign_tenant_sees_nothing() {
    let app = app().await;
    let created = app
        .create_upstream(json!({
            "alias": "mine.partner.com",
            "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
        }))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    // The owner reads its own upstream back.
    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);

    // Another tenant gets the same answer a missing upstream gets.
    let (status, _) = app
        .send_json_as(
            app.foreign,
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);

    // And its listing does not include it either.
    let (status, list) = app
        .send_json_as(
            app.foreign,
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    let aliases: Vec<&str> = list["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["alias"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !aliases.contains(&"mine.partner.com"),
        "a foreign tenant lists nothing of its neighbour: {list}"
    );
}

#[tokio::test]
async fn a_child_tenant_inherits_an_ancestors_inheritable_upstream() {
    let app = app().await;
    let created = app
        .create_upstream_as(
            app.parent,
            json!({
                "alias": "family.partner.com",
                "sharing": "inherit",
                "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
            }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    // The child addresses the ancestor's upstream through its own listing.
    let (status, list) = app
        .send_json_as(
            app.tenant,
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    let aliases: Vec<&str> = list["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["alias"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        aliases.contains(&"family.partner.com"),
        "an inheritable ancestor upstream is reachable from below: {list}"
    );

    // A private ancestor upstream is not.
    app.create_upstream_as(
        app.parent,
        json!({
            "alias": "secret.partner.com",
            "endpoints": [{"scheme": "https", "host": "b.partner.com"}]
        }),
    )
    .await;
    let (status, list) = app
        .send_json_as(
            app.tenant,
            http::Method::GET,
            "/oagw/v1/upstreams",
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    let aliases: Vec<&str> = list["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["alias"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(!aliases.contains(&"secret.partner.com"), "{list}");
    let _ = id;
}

#[tokio::test]
async fn a_list_is_paged_with_top_and_count() {
    let app = app().await;
    for index in 0..3 {
        app.create_upstream(json!({
            "alias": format!("page{index}.partner.com"),
            "endpoints": [{"scheme": "https", "host": format!("h{index}.partner.com")}]
        }))
        .await;
    }
    let (status, page) = app
        .send_json(http::Method::GET, "/oagw/v1/upstreams?$top=2", None, &[])
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        page["total_count"], 3,
        "the total is counted before paging: {page}"
    );
}

#[tokio::test]
async fn an_upstream_is_replaced_without_changing_its_alias() {
    let app = app().await;
    let created = app
        .create_upstream(json!({
            "alias": "replace.partner.com",
            "name": "Before",
            "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
        }))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, document) = app
        .send_json(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "alias": "replace.partner.com",
                "name": "After",
                "endpoints": [{"scheme": "https", "host": "b.partner.com"}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(document["name"], "After");
    assert_eq!(document["endpoints"][0]["host"], "b.partner.com");
    assert_eq!(document["alias"], "replace.partner.com");

    // An alias change is refused: it is the routing key.
    let (status, _) = app
        .send_json(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "alias": "moved.partner.com",
                "endpoints": [{"scheme": "https", "host": "b.partner.com"}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unreferenced_upstream_is_deletable() {
    let app = app().await;
    let created = app
        .create_upstream(json!({
            "alias": "delete.me.partner.com",
            "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
        }))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let response = app
        .send(app.request(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);

    let (status, _) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_referenced_upstream_cannot_be_deleted() {
    let app = app().await;
    let (upstream, _) = app
        .wire(
            json!({
                "alias": "used.partner.com",
                "endpoints": [{"scheme": "https", "host": "a.partner.com"}]
            }),
            "/v1/payments",
        )
        .await;
    let id = upstream["id"].as_str().unwrap().to_owned();

    let response = app
        .send(app.request(
            http::Method::DELETE,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn http_is_a_legal_endpoint_scheme() {
    let app = app().await;
    let (status, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "loopback.local",
                "endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 8080}]
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);
    assert_eq!(document["endpoints"][0]["scheme"], "http");
}

#[tokio::test]
async fn an_unauthenticated_request_is_unauthorized() {
    let app = app().await;
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri("/oagw/v1/upstreams")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.send(request).await;
    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(oagw::api::rest::error::ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        Some(oagw::api::rest::error::ERROR_SOURCE_GATEWAY.to_owned())
    );
}

/// The problem document a gateway-generated error carries.
#[tokio::test]
async fn errors_carry_the_gateway_source_header() {
    let app = app().await;
    let response = app
        .send(app.request(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{}", uuid::Uuid::new_v4()),
            None,
            &[],
        ))
        .await;
    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("application/problem+json"),
        "gateway errors are problem documents: {content_type}"
    );
}

/// An upstream is created enabled by default and can be created disabled.
#[tokio::test]
async fn an_upstream_defaults_to_enabled_and_can_be_created_disabled() {
    let app = app().await;
    let (created, default_document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "name": "Default",
                "endpoints": [{"scheme": "https", "host": "default.partner.com"}]
            })),
            &[],
        )
        .await;
    assert_eq!(created, http::StatusCode::CREATED);
    assert_eq!(default_document["enabled"], true, "the default is enabled");

    let (disabled, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "name": "Maintenance",
                "endpoints": [{"scheme": "https", "host": "down.partner.com"}],
                "enabled": false
            })),
            &[],
        )
        .await;
    assert_eq!(disabled, http::StatusCode::CREATED);
    assert_eq!(document["enabled"], false);
}

/// A disabled upstream reads back as disabled and can be re-enabled by its
/// owner, which is the ordinary maintenance cycle.
#[tokio::test]
async fn a_disabled_upstream_reads_back_disabled_and_can_be_reenabled() {
    let app = app().await;
    let (_, document) = app
        .send_json(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(json!({
                "name": "Maintenance",
                "endpoints": [{"scheme": "https", "host": "down.partner.com"}],
                "enabled": false
            })),
            &[],
        )
        .await;
    let id = document["id"].as_str().unwrap().to_owned();

    let (_, read) = app
        .send_json(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(read["enabled"], false);

    let mut replacement = document;
    replacement["enabled"] = json!(true);
    for drop in ["id", "tenant_id", "created_at", "updated_at", "alias"] {
        replacement.as_object_mut().unwrap().remove(drop);
    }
    let (re_enabled, replaced) = app
        .send_json(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(replacement),
            &[],
        )
        .await;
    assert_eq!(re_enabled, http::StatusCode::OK, "{replaced}");
    assert_eq!(replaced["enabled"], true);
}

/// A tenant cannot re-enable an ancestor it does not own: the ancestor's
/// decision stands for every descendant (FR-8).
#[tokio::test]
async fn a_descendant_cannot_re_enable_an_ancestor_disabled_upstream() {
    let app = app().await;
    let ancestor = app
        .create_upstream_as(
            app.parent,
            json!({
                "alias": "ancestor.disabled",
                "name": "Ancestor upstream",
                "endpoints": [{"scheme": "https", "host": "svc.ancestor.com"}],
                "sharing": "inherit",
                "enabled": false
            }),
        )
        .await;
    assert_eq!(ancestor["enabled"], false);

    // The descendant inherits the upstream and sees it disabled.
    let id = ancestor["id"].as_str().unwrap().to_owned();
    let (_, child_view) = app
        .send_json_as(
            app.tenant,
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(child_view["enabled"], false, "{child_view}");

    let (status, refusal) = app
        .send_json_as(
            app.tenant,
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "alias": ancestor["alias"],
                "name": "Ancestor upstream",
                "endpoints": [{"scheme": "https", "host": "svc.ancestor.com"}],
                "sharing": "inherit",
                "enabled": true
            })),
            &[],
        )
        .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN, "{refusal}");
}
