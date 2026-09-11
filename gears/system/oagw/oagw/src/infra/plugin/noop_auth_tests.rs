#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the no-op auth plugin.

use serde_json::Value;
use uuid::Uuid;

use super::*;
use crate::domain::plugin::{AuthPlugin, ProxyRequest};

fn request() -> ProxyRequest {
    ProxyRequest {
        method: axum::http::Method::GET,
        path: "/anything".to_string(),
        query: "a=1".to_string(),
        headers: axum::http::HeaderMap::new(),
        body: bytes::Bytes::new(),
        tenant_id: Uuid::nil(),
        security: None,
    }
}

#[tokio::test]
async fn it_leaves_the_request_untouched() {
    let mut request = request();
    NoopAuthPlugin.authenticate(&mut request, &Value::Null)
        .await
        .unwrap();
    assert!(request.headers.is_empty(), "no header may be injected");
    assert_eq!(request.query, "a=1", "the query must stay as it was");
    assert_eq!(request.path, "/anything");
}

#[tokio::test]
async fn it_ignores_whatever_configuration_it_is_given() {
    for config in [
        Value::Null,
        Value::Object(serde_json::Map::new()),
        serde_json::json!({"secret_ref": "cred://ignored"}),
    ] {
        let mut request = request();
        NoopAuthPlugin.authenticate(&mut request, &config).await.unwrap();
        assert!(request.headers.is_empty());
    }
}

#[test]
fn it_advertises_the_catalogued_identifier() {
    assert_eq!(NoopAuthPlugin.id(), crate::domain::gts_helpers::AUTH_NOOP);
    assert_eq!(NoopAuthPlugin.plugin_type(), "noop");
}
