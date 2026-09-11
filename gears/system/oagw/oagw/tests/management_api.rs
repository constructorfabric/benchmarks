//! Management API: routes, request/response bodies, status codes,
//! validation and error semantics.
//!
//! Every request goes through the real router, so what these tests assert is
//! the wire contract, not an internal one.

use oagw::domain::gts_helpers as gts;
use oagw::test_support::{Harness, context_for, read_json};
use serde_json::{Value, json};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = gts::PROTOCOL_HTTP;

fn hostname_upstream() -> Value {
    json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
        "protocol": HTTP_PROTOCOL
    })
}

async fn create_upstream(
    harness: &Harness,
    ctx: &toolkit_security::SecurityContext,
    body: &Value,
) -> Value {
    let response = harness.post_json(ctx, "/oagw/v1/upstreams", body).await;
    assert_eq!(response.status(), 201, "create must answer 201");
    read_json(response).await
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_upstream_derives_the_alias_and_answers_201_with_a_location() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let response = harness
        .post_json(&ctx, "/oagw/v1/upstreams", &hostname_upstream())
        .await;

    assert_eq!(response.status(), 201);
    let location = response.headers()[http::header::LOCATION]
        .to_str()
        .expect("ascii")
        .to_owned();
    let body = read_json(response).await;
    let id = body["id"].as_str().expect("id");
    assert!(
        id.starts_with("gts.cf.core.oagw.upstream.v1~"),
        "the id must be an anonymous GTS identifier, got {id}"
    );
    assert_eq!(location, format!("/oagw/v1/upstreams/{id}"));
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["protocol"], HTTP_PROTOCOL);
    assert_eq!(body["tenant_id"], ctx.subject_tenant_id().to_string());
}

#[tokio::test]
async fn a_plaintext_endpoint_is_accepted_by_the_management_api() {
    // `allow_http_upstream` governs whether a plaintext *connection* is made;
    // which schemes the field accepts is a separate question, and `http` is
    // one of them.
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = create_upstream(
        &harness,
        &ctx,
        &json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "mock.local", "port": 80 } ] },
            "protocol": HTTP_PROTOCOL
        }),
    )
    .await;
    assert_eq!(created["alias"], "mock.local");
    assert_eq!(created["server"]["endpoints"][0]["scheme"], "http");
}

#[tokio::test]
async fn every_tls_family_scheme_is_accepted() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    for (index, scheme) in ["https", "wss", "wt"].iter().enumerate() {
        let created = create_upstream(
            &harness,
            &ctx,
            &json!({
                "server": { "endpoints": [
                    { "scheme": scheme, "host": format!("api{index}.example.com"), "port": 443 }
                ] },
                "protocol": HTTP_PROTOCOL
            }),
        )
        .await;
        assert_eq!(created["server"]["endpoints"][0]["scheme"], *scheme);
    }
}

#[tokio::test]
async fn a_user_supplied_alias_is_rejected_for_a_hostname_pool() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let mut body = hostname_upstream();
    body["alias"] = json!("my-openai");
    let response = harness.post_json(&ctx, "/oagw/v1/upstreams", &body).await;
    assert_eq!(response.status(), 400);
    let problem = read_json(response).await;
    assert_eq!(problem["type"], gts::ERR_VALIDATION);
    assert_eq!(problem["status"], 400);
}

#[tokio::test]
async fn an_ip_pool_requires_an_explicit_alias() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let pool = json!({
        "server": { "endpoints": [
            { "scheme": "https", "host": "10.0.1.1", "port": 443 },
            { "scheme": "https", "host": "10.0.1.2", "port": 443 }
        ] },
        "protocol": HTTP_PROTOCOL
    });
    let response = harness.post_json(&ctx, "/oagw/v1/upstreams", &pool).await;
    assert_eq!(response.status(), 400);

    let mut with_alias = pool;
    with_alias["alias"] = json!("my-internal-service");
    let created = create_upstream(&harness, &ctx, &with_alias).await;
    assert_eq!(created["alias"], "my-internal-service");
}

#[tokio::test]
async fn a_multi_host_pool_derives_the_common_suffix() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = create_upstream(
        &harness,
        &ctx,
        &json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "us.vendor.com", "port": 443 },
                { "scheme": "https", "host": "eu.vendor.com", "port": 443 }
            ] },
            "protocol": HTTP_PROTOCOL
        }),
    )
    .await;
    assert_eq!(created["alias"], "vendor.com");
}

#[tokio::test]
async fn a_non_standard_port_is_carried_into_the_alias() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = create_upstream(
        &harness,
        &ctx,
        &json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.openai.com", "port": 8443 }
            ] },
            "protocol": HTTP_PROTOCOL
        }),
    )
    .await;
    assert_eq!(created["alias"], "api.openai.com:8443");
}

#[tokio::test]
async fn a_duplicate_alias_conflicts() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let response = harness
        .post_json(&ctx, "/oagw/v1/upstreams", &hostname_upstream())
        .await;
    assert_eq!(response.status(), 409);
    let problem = read_json(response).await;
    assert_eq!(problem["type"], gts::ERR_CONFLICT);
}

#[tokio::test]
async fn missing_required_members_are_400() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    for body in [
        json!({ "protocol": HTTP_PROTOCOL }),
        json!({ "server": { "endpoints": [] }, "protocol": HTTP_PROTOCOL }),
        json!({ "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] } }),
    ] {
        let response = harness.post_json(&ctx, "/oagw/v1/upstreams", &body).await;
        assert_eq!(response.status(), 400, "body {body} must be refused");
    }
}

#[tokio::test]
async fn a_heterogeneous_pool_is_400() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let response = harness
        .post_json(
            &ctx,
            "/oagw/v1/upstreams",
            &json!({
                "server": { "endpoints": [
                    { "scheme": "https", "host": "a.vendor.com", "port": 443 },
                    { "scheme": "https", "host": "b.vendor.com", "port": 8443 }
                ] },
                "protocol": HTTP_PROTOCOL
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn get_put_delete_round_trip() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let id = created["id"].as_str().expect("id");
    let path = format!("/oagw/v1/upstreams/{id}");

    let fetched = read_json(harness.get(&ctx, &path).await).await;
    assert_eq!(fetched["id"], created["id"]);

    let mut replacement = hostname_upstream();
    replacement["tags"] = json!(["openai", "llm"]);
    let replaced = harness.put_json(&ctx, &path, &replacement).await;
    assert_eq!(replaced.status(), 200);
    let replaced = read_json(replaced).await;
    assert_eq!(replaced["tags"], json!(["llm", "openai"]));
    assert_eq!(replaced["created_at"], created["created_at"]);

    assert_eq!(harness.delete(&ctx, &path).await.status(), 204);
    assert_eq!(harness.get(&ctx, &path).await.status(), 404);
    assert_eq!(harness.delete(&ctx, &path).await.status(), 404);
}

#[tokio::test]
async fn a_bare_uuid_also_addresses_the_resource() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let uuid = created["id"]
        .as_str()
        .expect("id")
        .rsplit('~')
        .next()
        .expect("uuid");
    let response = harness
        .get(&ctx, &format!("/oagw/v1/upstreams/{uuid}"))
        .await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn an_unparseable_id_is_404_not_500() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let response = harness.get(&ctx, "/oagw/v1/upstreams/not-an-id").await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "application/problem+json"
    );
}

#[tokio::test]
async fn replace_rejects_an_endpoint_change_that_moves_the_alias() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let created = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let path = format!("/oagw/v1/upstreams/{}", created["id"].as_str().expect("id"));
    let response = harness
        .put_json(
            &ctx,
            &path,
            &json!({
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.anthropic.com", "port": 443 }
                ] },
                "protocol": HTTP_PROTOCOL
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn another_tenant_cannot_see_or_touch_the_resource() {
    let harness = Harness::new();
    let owner = context_for(Uuid::new_v4());
    let stranger = context_for(Uuid::new_v4());
    let created = create_upstream(&harness, &owner, &hostname_upstream()).await;
    let path = format!("/oagw/v1/upstreams/{}", created["id"].as_str().expect("id"));

    assert_eq!(harness.get(&stranger, &path).await.status(), 404);
    assert_eq!(harness.delete(&stranger, &path).await.status(), 404);
    assert_eq!(
        harness
            .put_json(&stranger, &path, &hostname_upstream())
            .await
            .status(),
        404
    );
    let listed = read_json(harness.get(&stranger, "/oagw/v1/upstreams").await).await;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn a_tenantless_caller_is_403() {
    let harness = Harness::new();
    let anonymous = toolkit_security::SecurityContext::anonymous();
    let response = harness
        .post_json(&anonymous, "/oagw/v1/upstreams", &hostname_upstream())
        .await;
    assert_eq!(response.status(), 403);
    let problem = read_json(response).await;
    assert_eq!(problem["status"], 403);
}

#[tokio::test]
async fn cors_credentials_with_a_wildcard_origin_is_refused_at_write_time() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let mut body = hostname_upstream();
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allow_credentials": true
    });
    assert_eq!(
        harness
            .post_json(&ctx, "/oagw/v1/upstreams", &body)
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn a_catalog_only_auth_plugin_is_refused() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    for id in [gts::BASIC_AUTH_PLUGIN_ID, gts::BEARER_AUTH_PLUGIN_ID] {
        let mut body = hostname_upstream();
        body["auth"] = json!({ "type": id });
        let response = harness.post_json(&ctx, "/oagw/v1/upstreams", &body).await;
        assert_eq!(response.status(), 400, "{id} has no implementation");
        let problem = read_json(response).await;
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("unknown auth plugin")),
            "detail should say why: {problem}"
        );
    }
}

#[tokio::test]
async fn a_catalog_only_chain_plugin_is_not_bindable() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    for id in [
        gts::TIMEOUT_GUARD_PLUGIN_ID,
        gts::CORS_GUARD_PLUGIN_ID,
        gts::LOGGING_TRANSFORM_PLUGIN_ID,
        gts::METRICS_TRANSFORM_PLUGIN_ID,
    ] {
        let mut body = hostname_upstream();
        body["plugins"] = json!({ "items": [id] });
        assert_eq!(
            harness
                .post_json(&ctx, "/oagw/v1/upstreams", &body)
                .await
                .status(),
            400,
            "{id} must not be bindable"
        );
    }
}

#[tokio::test]
async fn the_built_in_chain_plugins_are_bindable_in_both_forms() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let mut body = hostname_upstream();
    body["plugins"] = json!({
        "items": [
            gts::REQUEST_ID_TRANSFORM_PLUGIN_ID,
            {
                "plugin_ref": gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
                "config": { "required_request_headers": "x-correlation-id" }
            }
        ]
    });
    let created = create_upstream(&harness, &ctx, &body).await;
    let items = created["plugins"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["plugin_ref"], gts::REQUEST_ID_TRANSFORM_PLUGIN_ID);
    assert_eq!(
        items[1]["config"]["required_request_headers"],
        "x-correlation-id"
    );
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_crud_round_trip() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let upstream = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let upstream_id = upstream["id"].as_str().expect("id");

    let response = harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": upstream_id,
                "match": { "http": {
                    "methods": ["post"],
                    "path": "v1/chat/completions/",
                    "query_allowlist": ["stream"]
                } }
            }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let created = read_json(response).await;
    let id = created["id"].as_str().expect("id");
    assert!(id.starts_with("gts.cf.core.oagw.route.v1~"));
    assert_eq!(created["upstream_id"], upstream_id);
    assert_eq!(created["match_type"], "http");
    assert_eq!(created["match"]["http"]["path"], "/v1/chat/completions");
    assert_eq!(created["match"]["http"]["methods"], json!(["POST"]));
    assert_eq!(created["match"]["http"]["path_suffix_mode"], "append");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["priority"], 0);

    let path = format!("/oagw/v1/routes/{id}");
    assert_eq!(harness.get(&ctx, &path).await.status(), 200);
    assert_eq!(harness.delete(&ctx, &path).await.status(), 204);
    assert_eq!(harness.get(&ctx, &path).await.status(), 404);
}

#[tokio::test]
async fn a_route_on_an_unknown_upstream_is_400() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let response = harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": Uuid::new_v4().to_string(),
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn another_tenants_upstream_is_not_addressable_from_a_route() {
    let harness = Harness::new();
    let owner = context_for(Uuid::new_v4());
    let stranger = context_for(Uuid::new_v4());
    let upstream = create_upstream(&harness, &owner, &hostname_upstream()).await;
    let response = harness
        .post_json(
            &stranger,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": upstream["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn duplicate_match_rules_conflict() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let upstream = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let body = json!({
        "upstream_id": upstream["id"],
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    assert_eq!(
        harness
            .post_json(&ctx, "/oagw/v1/routes", &body)
            .await
            .status(),
        201
    );
    assert_eq!(
        harness
            .post_json(&ctx, "/oagw/v1/routes", &body)
            .await
            .status(),
        409
    );
}

#[tokio::test]
async fn a_route_upstream_id_cannot_be_moved() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let first = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let second = create_upstream(
        &harness,
        &ctx,
        &json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.anthropic.com", "port": 443 }
            ] },
            "protocol": HTTP_PROTOCOL
        }),
    )
    .await;
    let route = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/routes",
                &json!({
                    "upstream_id": first["id"],
                    "match": { "http": { "methods": ["GET"], "path": "/v1" } }
                }),
            )
            .await,
    )
    .await;
    let path = format!("/oagw/v1/routes/{}", route["id"].as_str().expect("id"));
    let response = harness
        .put_json(
            &ctx,
            &path,
            &json!({
                "upstream_id": second["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn an_unsupported_route_method_is_400() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let upstream = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    let response = harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": upstream["id"],
                "match": { "http": { "methods": ["TRACE"], "path": "/v1" } }
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn deleting_an_upstream_cascades_to_its_routes() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let upstream = create_upstream(&harness, &ctx, &hostname_upstream()).await;
    harness
        .post_json(
            &ctx,
            "/oagw/v1/routes",
            &json!({
                "upstream_id": upstream["id"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            }),
        )
        .await;
    harness
        .delete(
            &ctx,
            &format!(
                "/oagw/v1/upstreams/{}",
                upstream["id"].as_str().expect("id")
            ),
        )
        .await;
    let routes = read_json(harness.get(&ctx, "/oagw/v1/routes").await).await;
    assert_eq!(routes["items"].as_array().map(Vec::len), Some(0));
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_crud_and_source_retrieval() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let source = "def on_request(ctx):\n    return ctx.next()\n";
    let response = harness
        .post_json(
            &ctx,
            "/oagw/v1/plugins",
            &json!({
                "plugin_type": "guard",
                "name": "request_validator",
                "description": "Validates request headers and body size",
                "config_schema": { "type": "object" },
                "source_code": source
            }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let created = read_json(response).await;
    let id = created["id"].as_str().expect("id");
    assert!(id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
    assert_eq!(created["plugin_type"], "guard");
    assert!(
        created.get("source_code").is_none(),
        "the definition must not echo the source"
    );

    let fetched = read_json(harness.get(&ctx, &format!("/oagw/v1/plugins/{id}")).await).await;
    assert_eq!(fetched["name"], "request_validator");

    let source_response = harness
        .get(&ctx, &format!("/oagw/v1/plugins/{id}/source"))
        .await;
    assert_eq!(source_response.status(), 200);
    let body = read_json(source_response).await;
    assert_eq!(body["source_code"], source);
    assert_eq!(body["id"], id);

    assert_eq!(
        harness
            .delete(&ctx, &format!("/oagw/v1/plugins/{id}"))
            .await
            .status(),
        204
    );
}

#[tokio::test]
async fn a_referenced_plugin_cannot_be_deleted() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let plugin = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/plugins",
                &json!({
                    "plugin_type": "guard",
                    "name": "validator",
                    "source_code": "def on_request(ctx):\n    return ctx.next()\n"
                }),
            )
            .await,
    )
    .await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    let mut upstream = hostname_upstream();
    upstream["plugins"] = json!({ "items": [ plugin_id.clone() ] });
    create_upstream(&harness, &ctx, &upstream).await;

    let response = harness
        .delete(&ctx, &format!("/oagw/v1/plugins/{plugin_id}"))
        .await;
    assert_eq!(response.status(), 409);
    let problem = read_json(response).await;
    assert_eq!(problem["type"], gts::ERR_PLUGIN_IN_USE);
    assert_eq!(problem["plugin_id"], plugin_id);
    assert_eq!(
        problem["referenced_by"]["upstreams"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn a_duplicate_plugin_name_conflicts() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let body = json!({
        "plugin_type": "transform",
        "name": "redact_pii",
        "phases": ["on_response"],
        "source_code": "def on_response(ctx):\n    return ctx.next()\n"
    });
    assert_eq!(
        harness
            .post_json(&ctx, "/oagw/v1/plugins", &body)
            .await
            .status(),
        201
    );
    assert_eq!(
        harness
            .post_json(&ctx, "/oagw/v1/plugins", &body)
            .await
            .status(),
        409
    );
}

#[tokio::test]
async fn a_plugin_is_immutable() {
    // No PUT is registered, so the router itself refuses the method.
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let plugin = read_json(
        harness
            .post_json(
                &ctx,
                "/oagw/v1/plugins",
                &json!({
                    "plugin_type": "guard",
                    "name": "immutable",
                    "source_code": "def on_request(ctx):\n    return ctx.next()\n"
                }),
            )
            .await,
    )
    .await;
    let response = harness
        .put_json(
            &ctx,
            &format!("/oagw/v1/plugins/{}", plugin["id"].as_str().expect("id")),
            &json!({ "name": "renamed" }),
        )
        .await;
    assert_eq!(response.status(), 405);
}

// ---------------------------------------------------------------------------
// List query options
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_supports_filter_select_orderby_top_and_skip() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        create_upstream(
            &harness,
            &ctx,
            &json!({
                "server": { "endpoints": [ { "scheme": "https", "host": host, "port": 443 } ] },
                "protocol": HTTP_PROTOCOL
            }),
        )
        .await;
    }

    let all = read_json(harness.get(&ctx, "/oagw/v1/upstreams").await).await;
    assert_eq!(all["items"].as_array().map(Vec::len), Some(3));
    assert_eq!(all["page_info"]["limit"], 50);

    let filtered = read_json(
        harness
            .get(
                &ctx,
                "/oagw/v1/upstreams?$filter=alias%20eq%20%27b.example.com%27",
            )
            .await,
    )
    .await;
    assert_eq!(filtered["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(filtered["items"][0]["alias"], "b.example.com");

    let projected = read_json(
        harness
            .get(&ctx, "/oagw/v1/upstreams?$select=id,alias")
            .await,
    )
    .await;
    let first = projected["items"][0].as_object().expect("object");
    assert_eq!(first.len(), 2);

    let ordered = read_json(
        harness
            .get(&ctx, "/oagw/v1/upstreams?$orderby=alias%20desc")
            .await,
    )
    .await;
    assert_eq!(ordered["items"][0]["alias"], "c.example.com");

    let paged = read_json(
        harness
            .get(&ctx, "/oagw/v1/upstreams?$top=2&$skip=1&$orderby=alias")
            .await,
    )
    .await;
    assert_eq!(paged["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(paged["items"][0]["alias"], "b.example.com");
    assert_eq!(paged["page_info"]["limit"], 2);
}

#[tokio::test]
async fn a_malformed_query_option_is_400() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    for query in [
        "$filter=alias%20like%20%27x%27",
        "$orderby=alias%20sideways",
        "$top=101",
        "$skip=-1",
    ] {
        let response = harness
            .get(&ctx, &format!("/oagw/v1/upstreams?{query}"))
            .await;
        assert_eq!(response.status(), 400, "{query} must be refused");
    }
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_gateway_error_is_rfc_9457_with_a_gts_type_and_a_source_header() {
    let harness = Harness::new();
    let ctx = context_for(Uuid::new_v4());
    let response = harness.get(&ctx, "/oagw/v1/upstreams/not-an-id").await;
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "application/problem+json"
    );
    assert_eq!(response.headers()["x-oagw-error-source"], "gateway");
    let problem = read_json(response).await;
    for member in ["type", "title", "status", "detail", "instance"] {
        assert!(
            problem.get(member).is_some(),
            "the problem document must carry '{member}': {problem}"
        );
    }
    assert_eq!(problem["instance"], "/oagw/v1/upstreams/not-an-id");
    assert!(
        problem["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("gts.cf.core.errors.err.v1~cf.oagw.")),
        "the type must be an OAGW GTS error identifier: {problem}"
    );
}
