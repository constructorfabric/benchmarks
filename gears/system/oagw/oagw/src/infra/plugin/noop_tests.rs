//! Tests for [`crate::infra::plugin::noop`].

use uuid::Uuid;

use super::NoopAuthPlugin;
use crate::domain::plugin::{AUTH_PLUGIN_TYPE_ID, AuthPlugin, RequestContext, builtin};

fn request() -> RequestContext {
    RequestContext::builder()
        .method("GET")
        .alias("payments")
        .path("/v1/payments")
        .tenant_id(Uuid::from_u128(0x11))
        .build()
}

#[test]
fn noop_plugin_declares_the_adr_ids() {
    let plugin = NoopAuthPlugin;
    assert_eq!(plugin.id(), builtin::NOOP_AUTH);
    assert_eq!(plugin.plugin_type(), AUTH_PLUGIN_TYPE_ID);
    assert_eq!(NoopAuthPlugin::PLUGIN_ID, builtin::NOOP_AUTH);
    assert_eq!(NoopAuthPlugin::PLUGIN_TYPE, AUTH_PLUGIN_TYPE_ID);
}

#[tokio::test]
async fn noop_plugin_injects_nothing() {
    let mut ctx = request();
    NoopAuthPlugin.authenticate(&mut ctx).await.expect("noop");
    assert!(ctx.injected_headers.is_empty());
    assert!(ctx.injected_query.is_empty());
    assert!(ctx.security.is_none());
    assert!(ctx.request_id.is_none());
}
