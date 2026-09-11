//! Integration tests of the binding-time plugin rejection
//! (`cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
//!
//! An instance configuration that fails the resolved plugin's registered
//! `config_schema`, and every catalog-only identifier, is rejected at binding
//! time and no binding row is stored.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-identifier-resolution:p1

use oagw::test_support::permissive_surface;
use serde_json::{json, Value};
use uuid::Uuid;

const AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~";
const GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~";
const TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~";
const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const OAUTH2: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

fn upstream_body() -> Value {
    json!({
        "protocol": PROTOCOL_HTTP,
        "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
    })
}

/// The catalog-only identifiers are rejected at binding time as unresolvable,
/// and none of them triggers the core timeout, CORS, logging or metrics
/// behavior.
#[tokio::test]
async fn a_catalog_only_identifier_is_rejected_at_binding_time() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    for reference in [
        format!("{AUTH}cf.core.oagw.basic.v1"),
        format!("{AUTH}cf.core.oagw.bearer.v1"),
        format!("{GUARD}cf.core.oagw.timeout.v1"),
        format!("{GUARD}cf.core.oagw.cors.v1"),
        format!("{TRANSFORM}cf.core.oagw.logging.v1"),
        format!("{TRANSFORM}cf.core.oagw.metrics.v1"),
    ] {
        let mut payload = upstream_body();
        payload["plugins"] = json!({ "sharing": "private", "items": [reference] });
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
        assert_eq!(status, http::StatusCode::BAD_REQUEST, "{reference}: {bytes:?}");
        let problem: Value = serde_json::from_slice(&bytes).expect("problem");
        assert_eq!(problem["status"], 400, "{reference}");
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 0, "no rejected write stored a record");
    assert_eq!(counts["oagw_upstream_plugin"], 0, "no binding row is stored");
}

/// An instance configuration that carries an unknown key is rejected.
#[tokio::test]
async fn an_unknown_config_key_is_rejected() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["auth"] = json!({
        "type": format!("{AUTH}cf.core.oagw.apikey.v1"),
        "config": { "api_key_ref": "cred://partner-openai-key", "unknown_key": true }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert_eq!(
        problem["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        "{problem}"
    );
    assert_eq!(surface.gear.storage().expect("storage").row_counts()["oagw_upstream"], 0);
}

/// A missing required key such as `client_id_ref` is rejected.
#[tokio::test]
async fn a_missing_required_key_is_rejected() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["auth"] = json!({
        "type": OAUTH2,
        "config": { "client_id_ref": "cred://partner-openai-key" }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    let detail = problem["detail"].as_str().expect("detail");
    assert!(detail.contains("client_secret_ref"), "{problem}");
}

/// A mutually exclusive key pair such as `token_endpoint` together with
/// `issuer_url` is rejected.
#[tokio::test]
async fn a_mutually_exclusive_key_pair_is_rejected() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["auth"] = json!({
        "type": OAUTH2,
        "config": {
            "client_id_ref": "cred://partner-openai-key",
            "client_secret_ref": "cred://partner-openai-secret",
            "token_endpoint": "https://idp.example.com/token",
            "issuer_url": "https://idp.example.com"
        }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    let detail = problem["detail"].as_str().expect("detail");
    assert!(detail.contains("token_endpoint"), "{problem}");
}

/// A credential-bearing field that is not a `cred://` reference is rejected,
/// and the rejected value is never echoed.
#[tokio::test]
async fn a_non_cred_reference_is_rejected_without_an_echo() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["auth"] = json!({
        "type": format!("{AUTH}cf.core.oagw.apikey.v1"),
        "config": { "api_key_ref": "sk-live-0123456789abcdef" }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let wire = String::from_utf8_lossy(&bytes).to_string();
    assert!(!wire.contains("sk-live-0123456789abcdef"), "no echo of the value: {wire}");
}

/// A guard binding whose required-header entries are all blank is rejected.
#[tokio::test]
async fn a_guard_with_blank_required_headers_is_rejected() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["plugins"] = json!({
        "sharing": "private",
        "items": [REQUIRED_HEADERS]
    });
    payload["auth"] = json!({
        "type": REQUIRED_HEADERS,
        "config": { "headers": [] }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert_eq!(
        problem["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        "{problem}"
    );
    assert_eq!(surface.gear.storage().expect("storage").row_counts()["oagw_upstream_plugin"], 0);
}

/// A schema-valid configuration is accepted and the binding rows are stored.
#[tokio::test]
async fn a_schema_valid_configuration_is_accepted() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["auth"] = json!({
        "type": format!("{AUTH}cf.core.oagw.apikey.v1"),
        "config": {
            "api_key_ref": "cred://partner-openai-key",
            "location": "header",
            "name": "x-api-key"
        }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 1);
}

/// A reference that resolves to no plugin of the calling tenant is rejected at
/// binding time.
#[tokio::test]
async fn an_unresolvable_custom_reference_is_rejected() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let absent = Uuid::new_v4();
    let mut payload = upstream_body();
    payload["plugins"] = json!({
        "sharing": "private",
        "items": [format!("{GUARD}{absent}")]
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert_eq!(
        problem["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        "{problem}"
    );
    // The rejection names the offending entry of `plugins.items[]` and stores
    // nothing — neither the upstream record nor a binding row.
    let detail = problem["detail"].as_str().expect("detail");
    assert!(detail.contains("plugins.items"), "{problem}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream"], 0);
    assert_eq!(counts["oagw_upstream_plugin"], 0);
}
