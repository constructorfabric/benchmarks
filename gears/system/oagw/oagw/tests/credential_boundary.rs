//! Integration tests for the credential-reference boundary
//! (`cpt-cf-oagw-dod-gear-foundation-credential-boundary`,
//! `cpt-cf-oagw-algo-gear-foundation-credential-boundary`).
//!
//! The gear persists through the service it publishes at initialization, so
//! every rejection here is observed through that published surface: a
//! credential-bearing field that does not hold a `cred://` reference is
//! rejected by name, and neither the stored record nor the rendered error ever
//! carries the secret material.
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-credential-boundary:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use toolkit::Gear;
use uuid::Uuid;

use oagw::test_support::{test_context, upstream};
use oagw::{DomainError, OagwGear};

const SECRET: &str = "sk-9f2b7c41-live-literal-bearer-token";

/// A service over the gear's own store, so the boundary is observed where the
/// REST handlers of entry 2.2 will call it.
async fn service() -> std::sync::Arc<dyn oagw::ControlPlaneService> {
    let gear = OagwGear::default();
    gear.init(&test_context(None)).await.expect("init succeeds");
    gear.service().expect("service published")
}

/// A raw secret in an `auth.config` credential-bearing field is rejected, and
/// the rejection names the field without echoing the value.
#[tokio::test]
async fn a_raw_secret_in_an_auth_config_field_is_rejected_without_being_echoed() {
    let service = service().await;
    let tenant = Uuid::new_v4();
    let mut record = upstream(tenant, "vendor.io");
    record.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: Some("custom-auth".to_owned()),
        config: Some(serde_json::json!({ "client_secret": SECRET })),
        ..oagw::AuthConfig::default()
    });

    let error = service.create_upstream(tenant, record).expect_err("rejected");
    let rendered = error.to_string();
    assert!(
        rendered.contains("credential-bearing fields accept a `cred://` reference only"),
        "{rendered}"
    );
    assert!(rendered.contains("auth.config.client_secret"), "{rendered} names the field");
    assert!(!rendered.contains(SECRET), "{rendered} must not echo the rejected value");
    assert!(!rendered.contains("sk-"), "{rendered} must not carry the secret prefix");
    assert!(
        service.list_upstreams(tenant).expect("list").is_empty(),
        "the rejected record was not stored"
    );
}

/// A well-formed `cred://` reference is accepted and stored as a reference —
/// the resolution belongs to entry 2.6, so the reference survives as-is.
#[tokio::test]
async fn a_cred_reference_is_accepted_and_stored_as_a_reference() {
    let service = service().await;
    let tenant = Uuid::new_v4();
    let mut record = upstream(tenant, "vendor.io");
    record.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: Some("custom-auth".to_owned()),
        config: Some(serde_json::json!({ "client_secret": "cred://tenant-a/openai" })),
        ..oagw::AuthConfig::default()
    });

    let created = service.create_upstream(tenant, record).expect("accepted");
    let config = created
        .auth
        .as_ref()
        .and_then(|auth| auth.config.as_ref())
        .expect("the config block is stored");
    assert_eq!(config["client_secret"].as_str(), Some("cred://tenant-a/openai"));
}

/// A `cred://`-shaped header value that is malformed is rejected by name; a
/// literal header value is configuration and stands.
#[tokio::test]
async fn a_malformed_cred_header_value_is_rejected_by_name() {
    let service = service().await;
    let tenant = Uuid::new_v4();
    let mut record = upstream(tenant, "vendor.io");
    record.headers = Some(oagw::HeadersConfig {
        request: Some(oagw::RequestHeaders {
            set: Some(
                [("authorization".to_owned(), "Bearer ".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            ..oagw::RequestHeaders::default()
        }),
        response: None,
    });

    let error = service.create_upstream(tenant, record).expect_err("rejected");
    let rendered = error.to_string();
    assert!(rendered.contains("headers.request.set.authorization"), "{rendered}");
    assert!(!rendered.contains("Bearer "), "{rendered} must not echo the rejected value");
}

/// A nested credential-bearing key inside an `auth.config` object is inspected
/// too, and the reported path is the full field path.
#[tokio::test]
async fn a_nested_secret_key_is_inspected_and_named_by_path() {
    let service = service().await;
    let tenant = Uuid::new_v4();
    let mut record = upstream(tenant, "vendor.io");
    record.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: Some("custom-auth".to_owned()),
        config: Some(serde_json::json!({
            "oauth": { "password_ref": "xoxb-not-a-reference" }
        })),
        ..oagw::AuthConfig::default()
    });

    let error = service.create_upstream(tenant, record).expect_err("rejected");
    let rendered = error.to_string();
    assert!(rendered.contains("auth.config.oauth.password_ref"), "{rendered}");
    assert!(!rendered.contains("xoxb"), "{rendered} must not echo the rejected value");
}

/// A replacement carrying secret material is rejected as well, so an update
/// cannot smuggle a secret past the boundary.
#[tokio::test]
async fn a_replacement_cannot_introduce_secret_material() {
    let service = service().await;
    let tenant = Uuid::new_v4();
    let mut stored = upstream(tenant, "vendor.io");
    stored.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: Some("custom-auth".to_owned()),
        config: Some(serde_json::json!({ "api_key_ref": "cred://tenant-a/openai" })),
        ..oagw::AuthConfig::default()
    });
    service.create_upstream(tenant, stored.clone()).expect("created");

    let stored_id = stored.id;
    let mut smuggled = stored;
    smuggled
        .auth
        .as_mut()
        .expect("auth present")
        .config = Some(serde_json::json!({ "api_key_ref": SECRET }));

    let error = service.replace_upstream(tenant, smuggled).expect_err("rejected");
    assert!(error.to_string().contains("api_key_ref"), "{}", error.to_string());
    let after = service.get_upstream(tenant, stored_id).expect("the record stands");
    let config = after
        .auth
        .as_ref()
        .and_then(|auth| auth.config.as_ref())
        .and_then(serde_json::Value::as_object)
        .expect("the config block is stored");
    assert_eq!(
        config["api_key_ref"].as_str(),
        Some("cred://tenant-a/openai"),
        "the stored reference was not overwritten"
    );
}

/// No error of the taxonomy ever renders a rejected value: the field-rejection
/// detail is a fixed sentence plus the field path.
#[test]
fn the_field_rejection_detail_never_carries_a_value() {
    for (field, reason) in [
        ("auth.config.client_secret_ref", "not a cred:// reference"),
        ("headers.request.set.authorization", "credential-bearing fields accept a `cred://` reference only"),
    ] {
        let error = DomainError::field_rejection(field, reason);
        let rendered = error.to_string();
        assert!(rendered.contains("rejected"), "{rendered}");
        assert!(rendered.contains(field), "{rendered}");
    }
}
