//! Unit tests for the no-op auth plugin.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::model::AuthConfig;
use crate::domain::plugin::AuthPlugin;
use crate::domain::plugin::test_support::{context, empty_resolver};
use std::collections::BTreeMap;

fn empty_config() -> AuthConfig {
    AuthConfig {
        plugin_type: crate::gts_helpers::AUTH_NOOP.to_owned(),
        sharing: crate::domain::model::SharingMode::default(),
        config: BTreeMap::new(),
    }
}

#[tokio::test]
async fn noop_injects_nothing() {
    let plugin = NoopAuthPlugin;
    let mut context = context("GET", "/things");
    let before = context.headers.clone();

    let decision = plugin
        .authenticate(&mut context, &empty_config(), &empty_resolver())
        .await
        .unwrap();

    assert_eq!(decision, AuthDecision::Passthrough);
    assert_eq!(context.headers, before);
    assert!(context.credential.is_none());
}

#[test]
fn noop_carries_the_built_in_identifier() {
    assert_eq!(NoopAuthPlugin.id(), crate::gts_helpers::AUTH_NOOP);
}
