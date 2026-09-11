//! Plugin context helpers and error projection.

use super::*;
use crate::domain::error::ErrorKind;

#[test]
fn a_request_renders_its_path_and_query() {
    let request = ProxyRequest {
        method: Method::GET,
        path: "/v1/chat".to_owned(),
        query: vec![
            ("model".to_owned(), "gpt-4".to_owned()),
            ("q".to_owned(), "a b&c".to_owned()),
        ],
        headers: HeaderMap::new(),
        body: Bytes::new(),
    };
    assert_eq!(request.path_and_query(), "/v1/chat?model=gpt-4&q=a+b%26c");
}

#[test]
fn an_empty_query_leaves_the_path_alone() {
    let request = ProxyRequest {
        method: Method::GET,
        path: "/v1/chat".to_owned(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Bytes::new(),
    };
    assert_eq!(request.path_and_query(), "/v1/chat");
}

#[test]
fn config_values_read_as_strings_whatever_their_json_type() {
    let mut config = ConfigMap::new();
    config.insert("text".to_owned(), "value".into());
    config.insert("number".to_owned(), 42.into());
    config.insert("flag".to_owned(), true.into());
    config.insert("nothing".to_owned(), serde_json::Value::Null);
    config.insert("blank".to_owned(), "   ".into());

    assert_eq!(config_str(&config, "text").as_deref(), Some("value"));
    assert_eq!(config_str(&config, "number").as_deref(), Some("42"));
    assert_eq!(config_str(&config, "flag").as_deref(), Some("true"));
    assert_eq!(config_str(&config, "nothing"), None);
    assert_eq!(config_str(&config, "absent"), None);

    // The non-blank reader additionally rejects whitespace-only values.
    assert_eq!(config_nonblank(&config, "blank"), None);
    assert_eq!(config_nonblank(&config, "text").as_deref(), Some("value"));
}

#[test]
fn plugin_errors_project_onto_the_catalogued_gateway_errors() {
    let cases = [
        (PluginError::Unauthenticated("x".into()), ErrorKind::AuthenticationFailed, 401),
        (PluginError::SecretNotFound("x".into()), ErrorKind::SecretNotFound, 500),
        (PluginError::InvalidConfig("x".into()), ErrorKind::ValidationError, 400),
        (PluginError::NotFound("x".into()), ErrorKind::PluginNotFound, 503),
        (PluginError::Internal("x".into()), ErrorKind::Internal, 500),
    ];
    for (error, kind, status) in cases {
        let projected: OagwError = error.into();
        assert_eq!(projected.kind, kind);
        assert_eq!(projected.status(), status);
    }
}

#[test]
fn a_guard_rejection_carries_its_status_and_code() {
    let decision = GuardDecision::reject(StatusCode::BAD_REQUEST, "MISSING", "no header");
    match decision {
        GuardDecision::Reject {
            status,
            error_code,
            message,
        } => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error_code, "MISSING");
            assert_eq!(message, "no header");
        }
        GuardDecision::Allow => panic!("expected a rejection"),
    }
}
