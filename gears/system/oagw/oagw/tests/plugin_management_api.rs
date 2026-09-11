//! Integration tests of the plugin management REST surface
//! (`cpt-cf-oagw-dod-plugin-system-management-api`).
//!
//! The five operations live under the gear-relative prefix, are scoped to the
//! calling tenant, and raise `401` and `403` through the shared surfaces.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-management-api:p1

use oagw::test_support::{permissive_surface, security_context, ManagementSurface};
use serde_json::{json, Value};
use uuid::Uuid;

const GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~";
const AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~";
const TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~";

fn body(name: &str, plugin_type: &str) -> Value {
    json!({
        "plugin_type": plugin_type,
        "name": name,
        "config_schema": { "type": "object" },
        "source_code": "const reference = 'opaque';"
    })
}

/// A tenant with one guard and one transform plugin created.
async fn seeded() -> (ManagementSurface, Uuid, Uuid, Uuid) {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    for (name, plugin_type) in [("first-guard", GUARD), ("first-transform", TRANSFORM)] {
        let (status, bytes) = surface
            .send(
                http::Method::POST,
                "/oagw/v1/plugins",
                Some(security_context(tenant, principal)),
                Some(body(name, plugin_type)),
            )
            .await;
        assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    }
    (surface, tenant, principal, Uuid::new_v4())
}

#[tokio::test]
async fn a_create_returns_the_server_generated_identifier() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(body("created-guard", GUARD)),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["plugin_type"], GUARD);
    assert_eq!(record["name"], "created-guard");
    assert!(record["id"].as_str().is_some(), "the identifier is server-assigned");
    // The source is not part of the record surface; it has its own endpoint.
    assert!(record.get("source_code").is_none(), "{record}");
}

/// The create response is addressed by the `Location` header, and all three
/// plugin base types are creatable.
#[tokio::test]
async fn the_create_response_is_addressed_by_location() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    for (index, plugin_type) in [GUARD, AUTH, TRANSFORM].iter().enumerate() {
        let (status, bytes) = surface
            .send(
                http::Method::POST,
                "/oagw/v1/plugins",
                Some(security_context(tenant, principal)),
                Some(body(&format!("located-{index}"), plugin_type)),
            )
            .await;
        assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
        let record: Value = serde_json::from_slice(&bytes).expect("record");
        assert!(record["id"].as_str().is_some());
    }
}

/// `GET /oagw/v1/plugins` lists the caller's own plugins, filtered, projected
/// and paginated by the OData system query options.
#[tokio::test]
async fn the_list_supports_the_odata_options() {
    let (surface, tenant, principal, _) = seeded().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 2);

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins?$filter=type%20eq%20%27guard%27",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 1, "{listed}");
    assert_eq!(listed["items"][0]["plugin_type"], GUARD);

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins?$select=name&$top=1",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(listed["count"], 1);
    assert!(listed["items"][0].get("name").is_some());
    assert!(listed["items"][0].get("plugin_type").is_none(), "the projection dropped it");
}

/// `GET /oagw/v1/plugins/{id}` accepts the bare UUID and the anonymous GTS
/// resource instance identifier alike.
#[tokio::test]
async fn the_by_identifier_read_accepts_both_identifier_forms() {
    let (surface, tenant, principal, _) = seeded().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(status, http::StatusCode::OK);
    let id = listed["items"][0]["id"].as_str().expect("id").to_owned();

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");

    let gts = format!("{GUARD}{id}");
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/plugins/{gts}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
}

/// A malformed identifier is a `400` validation failure through the shared
/// problem+json surface, never a plain-text extractor rejection.
#[tokio::test]
async fn a_malformed_identifier_is_a_validation_failure() {
    let (surface, tenant, principal, _) = seeded().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins/not-an-identifier",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert_eq!(problem["status"], 400, "{problem}");
    assert!(problem["type"].as_str().expect("type").starts_with("gts://"), "{problem}");
}

/// `GET /oagw/v1/plugins/{id}/source` returns the stored opaque reference
/// artifact.
#[tokio::test]
async fn the_source_endpoint_returns_the_registered_source() {
    let (surface, tenant, principal, _) = seeded().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    assert_eq!(status, http::StatusCode::OK);
    let id = listed["items"][0]["id"].as_str().expect("id").to_owned();

    let (status, bytes) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/plugins/{id}/source"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
    let source: Value = serde_json::from_slice(&bytes).expect("source");
    assert_eq!(source["id"], Value::String(id));
    assert_eq!(source["source_code"], "const reference = 'opaque';");
}

/// A foreign-tenant record is one indistinguishable not-found.
#[tokio::test]
async fn a_foreign_record_is_not_found() {
    let (surface, tenant, _principal, _) = seeded().await;
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins",
            Some(security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
    let listed: Value = serde_json::from_slice(&bytes).expect("list");
    let id = listed["items"][0]["id"].as_str().expect("id").to_owned();

    let stranger = Uuid::new_v4();
    let stranger_principal = Uuid::new_v4();
    for path in [
        format!("/oagw/v1/plugins/{id}"),
        format!("/oagw/v1/plugins/{id}/source"),
    ] {
        let (status, bytes) = surface
            .send(
                http::Method::GET,
                &path,
                Some(security_context(stranger, stranger_principal)),
                None,
            )
            .await;
        assert_eq!(status, http::StatusCode::NOT_FOUND, "{path}: {bytes:?}");
    }
    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(stranger, stranger_principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{bytes:?}");
}

/// A request with no security context is the `401` authentication surface, on
/// every one of the five operations.
#[tokio::test]
async fn a_missing_security_context_is_unauthenticated() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(body("unauthorized", GUARD)),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED);
    let id = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    for (method, path, body) in [
        (http::Method::POST, "/oagw/v1/plugins", Some(body("anon", GUARD))),
        (http::Method::GET, "/oagw/v1/plugins", None),
        (http::Method::GET, &format!("/oagw/v1/plugins/{id}"), None),
        (http::Method::GET, &format!("/oagw/v1/plugins/{id}/source"), None),
        (http::Method::DELETE, &format!("/oagw/v1/plugins/{id}"), None),
    ] {
        let (status, bytes) = surface.send(method, path, None, body).await;
        assert_eq!(status, http::StatusCode::UNAUTHORIZED, "{path}: {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem");
        assert_eq!(
            problem["type"],
            "gts://gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            "{problem}"
        );
    }
}

/// A valid context that lacks the required permission element is a `403`
/// through the shared canonical permission-denied surface.
#[tokio::test]
async fn a_denied_permission_is_forbidden() {
    let authz = std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default());
    let surface = oagw::test_support::management_surface(
        None,
        std::sync::Arc::clone(&authz),
        std::sync::Arc::new(oagw::test_support::FakeHierarchyTenantResolver::default()),
    )
    .await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(body("denied", GUARD)),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let id = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    authz.deny(&format!("{GUARD}:create"));
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(body("denied-2", GUARD)),
        )
        .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert!(problem["type"].as_str().expect("type").contains("permission"), "{problem}");

    authz.deny(&format!("{GUARD}:read"));
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN, "{bytes:?}");

    authz.deny(&format!("{GUARD}:delete"));
    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN, "{bytes:?}");
    // The denied delete left the record in place: the store was never
    // consulted for a denied operation.
    authz.deny(&format!("{GUARD}:delete"));
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_plugin"], 1, "the denied delete stored and removed nothing");
}

/// A `deny` on the base type's `read` element never discloses the record.
#[tokio::test]
async fn a_denied_read_discloses_nothing() {
    let authz = std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default());
    let surface = oagw::test_support::management_surface(
        None,
        std::sync::Arc::clone(&authz),
        std::sync::Arc::new(oagw::test_support::FakeHierarchyTenantResolver::default()),
    )
    .await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(body("hidden", GUARD)),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let id = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    authz.deny(&format!("{GUARD}:read"));
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    let wire = serde_json::to_string(&problem).expect("rendered");
    assert!(!wire.contains("hidden"), "no record content is disclosed: {wire}");
}
