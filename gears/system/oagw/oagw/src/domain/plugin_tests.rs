//! Tests of the plugin identification model.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use uuid::uuid;

use super::*;

const TENANT: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000f1");
const PLUGIN_ID: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000f2");

#[test]
fn the_three_plugin_types_are_gts_types() {
    for kind in PluginKind::ALL {
        assert!(kind.gts_type().ends_with('~'), "{kind} must end with '~'");
        assert!(gts::GtsTypeId::try_new(kind.gts_type()).is_ok(), "{kind}");
    }
    assert_eq!(
        PluginKind::Auth.gts_type(),
        "gts.cf.core.oagw.auth_plugin.v1~"
    );
    assert_eq!(
        PluginKind::Guard.gts_type(),
        "gts.cf.core.oagw.guard_plugin.v1~"
    );
    assert_eq!(
        PluginKind::Transform.gts_type(),
        "gts.cf.core.oagw.transform_plugin.v1~"
    );
}

#[test]
fn plugin_kinds_parse_from_tokens_and_gts_types() {
    assert_eq!(PluginKind::parse("auth").unwrap(), PluginKind::Auth);
    assert_eq!(PluginKind::parse("GUARD").unwrap(), PluginKind::Guard);
    assert_eq!(
        PluginKind::parse("transform").unwrap(),
        PluginKind::Transform
    );
    assert_eq!(
        PluginKind::parse(AUTH_PLUGIN_TYPE).unwrap(),
        PluginKind::Auth
    );
    assert!(PluginKind::parse("filter").is_err());
    assert_eq!(PluginKind::Auth.to_string(), "auth");
    assert_eq!(PluginKind::Guard.execution_rank(), 1);
    assert!(PluginKind::Auth.execution_rank() < PluginKind::Guard.execution_rank());
    assert!(PluginKind::Guard.execution_rank() < PluginKind::Transform.execution_rank());
}

#[test]
fn builtin_plugin_identifiers_are_valid_gts_instances() {
    for id in BINDABLE_PLUGINS
        .iter()
        .copied()
        .chain(CATALOG_ONLY_PLUGINS.iter().copied())
    {
        assert!(
            gts::GtsInstanceId::try_new(id).is_ok(),
            "'{id}' must be a valid GTS instance id"
        );
        assert!(!id.ends_with('~'), "'{id}' must not be a type id");
    }
}

#[test]
fn builtin_plugins_bind_to_their_own_type() {
    let apikey = PluginRef::builtin(PluginKind::Auth, AUTH_APIKEY).unwrap();
    assert_eq!(apikey.kind(), PluginKind::Auth);
    assert_eq!(
        apikey.instance(),
        &PluginInstance::Named(AUTH_APIKEY.to_owned())
    );
    assert!(apikey.is_builtin());
    assert_eq!(apikey.to_string(), AUTH_APIKEY);
    assert!(PluginRef::builtin(PluginKind::Guard, AUTH_APIKEY).is_err());
}

#[test]
fn plugin_references_accept_gts_ids_and_uuids() {
    let guard = PluginRef::parse(PluginKind::Guard, GUARD_REQUIRED_HEADERS).unwrap();
    assert!(guard.is_builtin());
    assert_eq!(guard.plugin_uuid(), None);
    assert!(!guard.uuid_matches(PLUGIN_ID));
    let custom = PluginRef::parse(PluginKind::Transform, PLUGIN_ID.to_string().as_str()).unwrap();
    assert!(!custom.is_builtin());
    assert_eq!(custom.plugin_uuid(), Some(PLUGIN_ID));
    assert!(custom.uuid_matches(PLUGIN_ID));
    assert_eq!(custom.as_ref_str(), PLUGIN_ID.as_simple().to_string());
    assert!(PluginRef::parse(PluginKind::Auth, "not-a-plugin").is_err());
}

#[test]
fn a_plugin_reference_must_match_its_declared_kind() {
    // A transform plugin id does not parse as an auth plugin reference.
    assert!(
        PluginRef::parse(PluginKind::Auth, TRANSFORM_REQUEST_ID).is_err(),
        "the GTS type of the reference must match the declared kind"
    );
    assert!(PluginRef::builtin(PluginKind::Auth, TRANSFORM_REQUEST_ID).is_err());
}

#[test]
fn catalog_only_plugins_are_not_bindable() {
    let kind_of = |id: &str| {
        PluginKind::ALL
            .iter()
            .copied()
            .find(|kind| id.starts_with(kind.gts_type()))
            .unwrap_or_else(|| panic!("'{id}' must start with a plugin type id"))
    };
    for id in CATALOG_ONLY_PLUGINS {
        let reference = PluginRef::parse(kind_of(id), id).unwrap();
        assert!(!reference.is_bindable(), "'{id}' is catalog-only");
    }
    for id in BINDABLE_PLUGINS {
        let reference = PluginRef::parse(kind_of(id), id).unwrap();
        assert!(reference.is_bindable(), "'{id}' must be bindable");
    }
    // Custom (Starlark) plugins are always bindable.
    let custom = PluginRef::parse(PluginKind::Guard, PLUGIN_ID.to_string().as_str()).unwrap();
    assert!(custom.is_bindable());
}

#[test]
fn the_builtin_catalog_covers_every_documented_plugin() {
    assert_eq!(BINDABLE_PLUGINS.len(), 6);
    assert_eq!(CATALOG_ONLY_PLUGINS.len(), 6);
    for id in BINDABLE_PLUGINS {
        assert!(
            !CATALOG_ONLY_PLUGINS.contains(&id),
            "{id} appears in both lists"
        );
    }
    assert!(BINDABLE_PLUGINS.contains(&AUTH_NOOP));
    assert!(BINDABLE_PLUGINS.contains(&AUTH_APIKEY));
    assert!(BINDABLE_PLUGINS.contains(&AUTH_OAUTH2_CLIENT_CRED));
    assert!(BINDABLE_PLUGINS.contains(&AUTH_OAUTH2_CLIENT_CRED_BASIC));
    assert!(BINDABLE_PLUGINS.contains(&GUARD_REQUIRED_HEADERS));
    assert!(BINDABLE_PLUGINS.contains(&TRANSFORM_REQUEST_ID));
    assert!(CATALOG_ONLY_PLUGINS.contains(&AUTH_BASIC));
    assert!(CATALOG_ONLY_PLUGINS.contains(&AUTH_BEARER));
    assert!(CATALOG_ONLY_PLUGINS.contains(&GUARD_TIMEOUT));
    assert!(CATALOG_ONLY_PLUGINS.contains(&GUARD_CORS));
    assert!(CATALOG_ONLY_PLUGINS.contains(&TRANSFORM_LOGGING));
    assert!(CATALOG_ONLY_PLUGINS.contains(&TRANSFORM_METRICS));
}

#[test]
fn plugin_phases_execute_in_the_documented_order() {
    let phases = [
        PluginPhase::Auth,
        PluginPhase::Guard,
        PluginPhase::RequestTransform,
        PluginPhase::ResponseTransform,
        PluginPhase::ErrorTransform,
    ];
    for pair in phases.windows(2) {
        assert!(
            pair[0].rank() < pair[1].rank(),
            "{:?} must run before {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn custom_plugin_descriptors_are_immutable_once_created() {
    let plugin = Plugin::new(
        PLUGIN_ID,
        TENANT,
        PluginKind::Transform,
        String::from("redact"),
        String::from("def run(req):\n    return req"),
    )
    .unwrap();
    assert_eq!(plugin.id, PLUGIN_ID);
    assert_eq!(plugin.tenant_id, TENANT);
    assert_eq!(plugin.kind, PluginKind::Transform);
    assert_eq!(plugin.gts_type(), TRANSFORM_PLUGIN_TYPE);
    assert!(!plugin.gc_eligible);
    assert!(
        Plugin::new(
            PLUGIN_ID,
            TENANT,
            PluginKind::Auth,
            String::from("  "),
            String::from("def run(): pass")
        )
        .is_err()
    );
    assert!(
        Plugin::new(
            PLUGIN_ID,
            TENANT,
            PluginKind::Auth,
            String::from("name"),
            String::from(" ")
        )
        .is_err()
    );
}

#[test]
fn plugin_references_are_hashable_and_comparable() {
    let left = PluginRef::parse(PluginKind::Auth, AUTH_APIKEY).unwrap();
    let right = PluginRef::parse(PluginKind::Auth, AUTH_APIKEY).unwrap();
    assert_eq!(left, right);
    let mut seen = std::collections::HashSet::new();
    seen.insert(left);
    assert!(seen.contains(&right));
    // A custom plugin with the same textual id stays distinct.
    let custom = PluginRef::parse(PluginKind::Auth, PLUGIN_ID.to_string().as_str()).unwrap();
    seen.insert(custom.clone());
    assert_eq!(seen.len(), 2);
}
