#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::*;

/// The plugin ids are the binding key of the registries and the values the
/// configuration names, so they are pinned verbatim (`ADR`-0008, `ADR`-0009).
#[test]
fn plugin_ids_are_the_documented_gts_ids() {
    assert_eq!(
        NOOP_AUTH_PLUGIN_ID,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
    );
    assert_eq!(
        API_KEY_AUTH_PLUGIN_ID,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(
        OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1"
    );
    assert_eq!(
        OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
    );
    assert_eq!(
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(
        REQUEST_ID_TRANSFORM_PLUGIN_ID,
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
}

/// Every catalogued built-in of `DESIGN` §3.2 has a helper id when it is
/// resolvable, so the registry cannot drift from the catalog.
#[test]
fn every_bindable_builtin_has_a_resolvable_id() {
    for builtin in crate::domain::plugin::AUTH_BUILTINS
        .iter()
        .filter(|builtin| builtin.bindable)
    {
        assert!(
            builtin.gts_id == NOOP_AUTH_PLUGIN_ID
                || builtin.gts_id == API_KEY_AUTH_PLUGIN_ID
                || builtin.gts_id == OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
                || builtin.gts_id == OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            "{:?} has no helper id",
            builtin.name
        );
    }
    for builtin in crate::domain::plugin::GUARD_BUILTINS
        .iter()
        .filter(|builtin| builtin.bindable)
    {
        assert_eq!(builtin.gts_id, REQUIRED_HEADERS_GUARD_PLUGIN_ID);
    }
    for builtin in crate::domain::plugin::TRANSFORM_BUILTINS
        .iter()
        .filter(|builtin| builtin.bindable)
    {
        assert_eq!(builtin.gts_id, REQUEST_ID_TRANSFORM_PLUGIN_ID);
    }
}
