//! Unit tests for the `oagw.config` block (`cpt-cf-oagw-dod-gear-foundation-config-model`).
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-config-model:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-unit-tests:p1

use serde_json::json;

use super::*;
use crate::domain::dto::{HeaderPassthrough, RequestHeaders, ResponseHeaders};

#[test]
fn every_documented_default_is_verbatim() {
    let config = OagwConfig::default();
    assert!(!config.allow_http_upstream);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
    assert_eq!(config.ssrf_policy, SsrfPolicy::Disabled);
    assert_eq!(config.max_body_size_bytes, 100 * 1024 * 1024);
    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.validate().is_ok(), "the defaults are valid");
}

#[test]
fn an_absent_block_yields_the_defaults() {
    let config = OagwConfig::from_value(None).expect("defaults");
    assert_eq!(config, OagwConfig::default());
}

#[test]
fn a_block_parses_and_validates() {
    let config = OagwConfig::from_value(Some(&json!({
        "allow_http_upstream": true,
        "token_cache_ttl_secs": 60,
        "token_cache_capacity": 100,
        "ssrf_policy": "disabled",
        "max_body_size_bytes": 1048576,
        "proxy_timeout_secs": 5
    })))
    .expect("parses");
    assert!(config.allow_http_upstream);
    assert_eq!(config.token_cache_config().ttl_secs, 60);
    assert_eq!(config.token_cache_config().capacity, 100);
}

#[test]
fn an_unknown_key_is_rejected() {
    let error = OagwConfig::from_value(Some(&json!({
        "allow_http_upstream": true,
        "no_such_key": 1
    })))
    .expect_err("unknown key rejected");
    assert!(error.to_string().contains("no_such_key"), "`{error}` names the key");
}

#[test]
fn non_positive_integers_are_rejected() {
    for key in ["proxy_timeout_secs", "token_cache_ttl_secs", "token_cache_capacity"] {
        let error = OagwConfig::from_value(Some(&json!({ key: 0 }))).expect_err("zero rejected");
        assert!(error.to_string().contains(key), "`{error}` names `{key}`");
    }
}

#[test]
fn a_body_limit_over_the_ceiling_is_rejected() {
    let error = OagwConfig::from_value(Some(&json!({
        "max_body_size_bytes": 100 * 1024 * 1024 + 1
    })))
    .expect_err("over the 100 MB ceiling");
    assert!(error.to_string().contains("100 MB ceiling"), "`{error}` names the ceiling");
    assert!(OagwConfig::from_value(Some(&json!({ "max_body_size_bytes": 100 * 1024 * 1024 })))
        .is_ok(), "exactly the ceiling is accepted");
}

#[test]
fn endpoint_schemes_are_restricted_to_the_documented_set_with_https_default() {
    for scheme in ["http", "https", "wss", "wt", "grpc"] {
        let endpoint: crate::domain::dto::Endpoint =
            serde_json::from_value(json!({ "scheme": scheme, "host": "api.vendor.com" }))
                .unwrap_or_else(|error| panic!("{scheme} is a legal scheme: {error}"));
        assert_eq!(endpoint.port, 443);
    }
    let endpoint: crate::domain::dto::Endpoint =
        serde_json::from_value(json!({ "host": "api.vendor.com" })).expect("parses");
    assert_eq!(endpoint.scheme, crate::domain::dto::EndpointScheme::Https, "https is the default");
    assert!(
        serde_json::from_value::<crate::domain::dto::Endpoint>(json!({
            "scheme": "ftp", "host": "api.vendor.com"
        }))
        .is_err(),
        "an unknown scheme is rejected"
    );
}

#[test]
fn a_credential_bearing_field_accepts_a_cred_reference_only() {
    // A well-formed `cred://` reference is accepted.
    let auth = AuthConfig {
        auth_type: Some(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: crate::domain::dto::SharingMode::Private,
        config: Some(json!({ "api_key_ref": "cred://tenant-a/openai" })),
    };
    assert!(reject_non_cred_reference_values("upstream", Some(&auth), None).is_ok());

    // A raw secret material is rejected, and the rejection never echoes it.
    let raw = AuthConfig {
        auth_type: Some(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: crate::domain::dto::SharingMode::Private,
        config: Some(json!({ "api_key_ref": "sk_live_51H8xYzAbCdEfGh" })),
    };
    let error = reject_non_cred_reference_values("upstream", Some(&raw), None)
        .expect_err("raw secret rejected");
    let text = error.to_string();
    assert!(text.contains("api_key_ref"), "`{text}` names the field");
    assert!(!text.contains("sk_live"), "`{text}` never echoes the rejected value");
}

#[test]
fn nested_credential_references_are_walked() {
    let nested = AuthConfig {
        auth_type: Some(crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned()),
        sharing: crate::domain::dto::SharingMode::Private,
        config: Some(json!({
            "token_url": "https://auth.vendor.com/oauth/token",
            "client_secret": "hunter2"
        })),
    };
    let error = reject_non_cred_reference_values("upstream", Some(&nested), None)
        .expect_err("raw client_secret rejected");
    assert!(error.to_string().contains("client_secret"));
}

#[test]
fn a_malformed_cred_uri_is_rejected() {
    let malformed = AuthConfig {
        auth_type: Some(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing: crate::domain::dto::SharingMode::Private,
        config: Some(json!({ "api_key_ref": "cred:/" })),
    };
    let error = reject_non_cred_reference_values("upstream", Some(&malformed), None)
        .expect_err("malformed reference rejected");
    assert!(error.to_string().contains("api_key_ref"));
}

#[test]
fn header_values_starting_with_the_cred_prefix_must_be_references() {
    let headers = HeadersConfig {
        request: Some(RequestHeaders {
            set: Some([("authorization".to_owned(), "cred://".to_owned())].into_iter().collect()),
            add: None,
            remove: None,
            passthrough: Some(HeaderPassthrough::None),
            passthrough_allowlist: None,
        }),
        response: Some(ResponseHeaders::default()),
    };
    let error = reject_non_cred_reference_values("upstream", None, Some(&headers))
        .expect_err("malformed header credential rejected");
    assert!(error.to_string().contains("headers.request.set.authorization"));

    let ok = HeadersConfig {
        request: Some(RequestHeaders {
            set: Some([("authorization".to_owned(), "cred://tenant-a/openai".to_owned())].into_iter().collect()),
            add: None,
            remove: None,
            passthrough: Some(HeaderPassthrough::None),
            passthrough_allowlist: None,
        }),
        response: Some(ResponseHeaders::default()),
    };
    assert!(reject_non_cred_reference_values("upstream", None, Some(&ok)).is_ok());
}

#[test]
fn a_literal_in_a_reference_shaped_oauth2_key_is_rejected() {
    // `client_secret_ref` / `client_id_ref` are the spellings the OAuth2
    // client-credentials plugin actually reads; a literal pasted into either
    // is secret material, not a reference.
    let raw = AuthConfig {
        auth_type: Some(crate::domain::gts_helpers::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned()),
        sharing: crate::domain::dto::SharingMode::Private,
        config: Some(json!({
            "token_url": "https://auth.vendor.com/oauth/token",
            "client_id_ref": "partner-gateway",
            "client_secret_ref": "a1b2c3d4e5f60718293a4b5c6d7e8f90"
        })),
    };
    let error = reject_non_cred_reference_values("upstream", Some(&raw), None)
        .expect_err("a literal client_secret_ref is rejected");
    let text = error.to_string();
    assert!(text.contains("client_secret_ref"), "`{text}` names the field");
    assert!(
        !text.contains("a1b2c3d4"),
        "`{text}` never echoes the rejected value"
    );
    // The non-credential `token_url` is still configuration and is not the
    // field the rejection names.
    assert!(!text.contains("token_url"), "`{text}` does not name token_url");
}

#[test]
fn a_credential_bearing_header_name_containing_a_dot_is_still_matched() {
    // A header name is a legal RFC 7230 token even when it carries dots, so
    // the boundary must not recover the name as the last dot-segment of the
    // field path.
    let headers = HeadersConfig {
        request: Some(RequestHeaders {
            set: Some([("x.api.key".to_owned(), "raw-secret-value".to_owned())].into_iter().collect()),
            add: None,
            remove: None,
            passthrough: Some(HeaderPassthrough::None),
            passthrough_allowlist: None,
        }),
        response: Some(ResponseHeaders::default()),
    };
    assert!(
        reject_non_cred_reference_values("upstream", None, Some(&headers)).is_ok(),
        "a dotted name outside the credential-bearing set is configuration"
    );

    let set_cookie = HeadersConfig {
        request: Some(RequestHeaders {
            set: Some([("set-cookie".to_owned(), "session=abc".to_owned())].into_iter().collect()),
            add: None,
            remove: None,
            passthrough: Some(HeaderPassthrough::None),
            passthrough_allowlist: None,
        }),
        response: Some(ResponseHeaders::default()),
    };
    let error = reject_non_cred_reference_values("upstream", None, Some(&set_cookie))
        .expect_err("a literal set-cookie value is rejected");
    assert!(error.to_string().contains("headers.request.set.set-cookie"));
}
