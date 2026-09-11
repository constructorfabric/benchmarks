//! Integration-level tests for DECOMPOSITION entry 2.4 (Plugin Management
//! API), exercising the crate's **public** surface as a black box
//! (`oagw::...`).
//!
//! Router-level HTTP tests for `POST`/`GET`/`DELETE /oagw/v1/plugins...`
//! (status codes, RFC 9457 bodies, the `409 PluginInUse` envelope) live
//! inline in `src/api/rest/plugins.rs`'s own `#[cfg(test)] mod tests`,
//! matching this crate's established pattern (`src/api/rest/route_api.rs`,
//! `src/api/rest/upstreams.rs`): `api::rest::plugins` and its `handlers`/
//! `dto` submodules are private to the crate (`api/rest/mod.rs` declares
//! `mod plugins;`, not `pub mod`, and is a file this feature does not own),
//! so an external `tests/` crate cannot reach `register_routes` or the
//! handlers directly. This file instead black-box-tests the plugin
//! identification/resolution/reference-counting/GC model this feature
//! exports publicly from `oagw::model::plugin` for reuse by 2.2/2.3/2.9.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use oagw::config::OagwConfig;
use oagw::model::plugin::{
    Plugin, PluginIdentifierError, PluginResolutionError, PluginType, count_plugin_references,
    lookup_plugin_for_management, mark_gc_eligibility, named_plugin_gts_ref,
    parse_plugin_identifier, plugin_gts_ref, resolve_plugin_ref,
};
use oagw::store::OagwState;
use uuid::Uuid;

fn sample_plugin(id: Uuid, tenant_id: Uuid, plugin_type: PluginType, name: &str) -> Plugin {
    Plugin {
        id: Some(id),
        tenant_id: Some(tenant_id),
        plugin_type,
        name: name.to_owned(),
        description: None,
        config_schema: None,
        source_code: "def guard(req): return req".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
    }
}

/// All twelve named-plugin catalog tokens classify as `kind: "named"` and
/// resolve successfully via `resolve_plugin_ref` without any stored row --
/// the acceptance criterion this feature's identification model must
/// satisfy for `cpt-cf-oagw-feature-plugin-execution` (2.9) to trust.
#[test]
fn all_twelve_named_catalog_tokens_classify_named_and_resolve_with_no_stored_row() {
    let tenant_id = Uuid::new_v4();
    let state = OagwState::new(OagwConfig::default());
    let mut total_tokens = 0;

    for plugin_type in [PluginType::Auth, PluginType::Guard, PluginType::Transform] {
        for entry in plugin_type.named_catalog() {
            total_tokens += 1;
            let candidate = named_plugin_gts_ref(plugin_type, entry.token);

            let parsed = parse_plugin_identifier(&candidate, None).unwrap();
            assert!(matches!(
                parsed,
                oagw::model::plugin::PluginIdentifier::Named { .. }
            ));

            let resolved =
                resolve_plugin_ref(&candidate, None, tenant_id, state.store.plugins()).unwrap();
            assert!(!resolved.storage_backed);
            assert_eq!(resolved.plugin_uuid, None);
        }
    }

    assert_eq!(total_tokens, 12);
}

/// `builtin_vs_catalog`: a catalog-only identifier (no backing trait
/// implementation per ADR-0002's prose, e.g. `basic`/`timeout`/`logging`)
/// still resolves via the identification algorithm (this feature's job),
/// but its `NamedPluginCatalogEntry::has_backing_implementation` is `false`
/// -- distinguishing it from a token like `apikey`/`required_headers`/
/// `request_id` that a plugin-execution engine could actually load.
#[test]
fn catalog_only_identifiers_resolve_for_identification_but_report_no_backing_implementation() {
    let tenant_id = Uuid::new_v4();
    let state = OagwState::new(OagwConfig::default());

    let catalog_only = [
        (PluginType::Auth, "basic"),
        (PluginType::Auth, "bearer"),
        (PluginType::Guard, "timeout"),
        (PluginType::Guard, "cors"),
        (PluginType::Transform, "logging"),
        (PluginType::Transform, "metrics"),
    ];
    for (plugin_type, token) in catalog_only {
        let candidate = named_plugin_gts_ref(plugin_type, token);
        let resolved =
            resolve_plugin_ref(&candidate, None, tenant_id, state.store.plugins()).unwrap();
        assert!(
            !resolved.storage_backed,
            "{token} should still classify as named"
        );

        let catalog_entry = plugin_type.named_catalog_entry(token).unwrap();
        assert!(
            !catalog_entry.has_backing_implementation,
            "{token} must be reported as catalog-only, not backed"
        );
    }

    let backed = [
        (PluginType::Auth, "apikey"),
        (PluginType::Guard, "required_headers"),
        (PluginType::Transform, "request_id"),
    ];
    for (plugin_type, token) in backed {
        let catalog_entry = plugin_type.named_catalog_entry(token).unwrap();
        assert!(
            catalog_entry.has_backing_implementation,
            "{token} must be reported as backed"
        );
    }
}

/// `id_form_check`: `lookup_plugin_for_management` (the shared model helper
/// backing `GET`/`GET .../source`/`DELETE /oagw/v1/plugins/{id}`) resolves
/// the same stored plugin identically whether `{id}` is supplied as the bare
/// UUID or as the anonymous GTS form.
#[test]
fn lookup_for_management_resolves_identically_for_both_id_forms() {
    let tenant_id = Uuid::new_v4();
    let state = OagwState::new(OagwConfig::default());
    let uuid = Uuid::new_v4();
    let plugin = sample_plugin(uuid, tenant_id, PluginType::Transform, "req-id-plugin");
    state.store.plugins().insert(uuid, Arc::new(plugin));

    let by_bare_uuid =
        lookup_plugin_for_management(&uuid.to_string(), tenant_id, state.store.plugins()).unwrap();
    let gts_form = plugin_gts_ref(PluginType::Transform, uuid);
    let by_gts_form =
        lookup_plugin_for_management(&gts_form, tenant_id, state.store.plugins()).unwrap();

    assert_eq!(by_bare_uuid.id, Some(uuid));
    assert_eq!(by_gts_form.id, Some(uuid));
    assert_eq!(by_bare_uuid.name, by_gts_form.name);
}

#[test]
fn a_malformed_identifier_fails_parsing_and_resolution() {
    let tenant_id = Uuid::new_v4();
    let state = OagwState::new(OagwConfig::default());

    assert_eq!(
        parse_plugin_identifier("definitely not an id", None).unwrap_err(),
        PluginIdentifierError::Malformed
    );
    assert_eq!(
        resolve_plugin_ref(
            "definitely not an id",
            None,
            tenant_id,
            state.store.plugins()
        )
        .unwrap_err(),
        PluginResolutionError::Malformed
    );
}

/// End-to-end model-layer scenario (create -> reference -> unreference)
/// exercising `count_plugin_references` and `mark_gc_eligibility` together,
/// purely through the crate's public API -- complementary to the HTTP-level
/// `409 PluginInUse` test inline in `src/api/rest/plugins.rs`.
#[test]
fn gc_eligibility_tracks_the_plugin_reference_lifecycle_end_to_end() {
    let tenant_id = Uuid::new_v4();
    let state = OagwState::new(OagwConfig::default());
    let plugin_uuid = Uuid::new_v4();
    let plugin_ref = plugin_gts_ref(PluginType::Guard, plugin_uuid);

    // Creation: zero references -> gc_eligible_at becomes non-null.
    let initial_gc_eligible_at = mark_gc_eligibility(None, 0);
    assert!(initial_gc_eligible_at.is_some());
    let plugin = Plugin {
        gc_eligible_at: initial_gc_eligible_at,
        ..sample_plugin(
            plugin_uuid,
            tenant_id,
            PluginType::Guard,
            "lifecycle-plugin",
        )
    };
    state.store.plugins().insert(plugin_uuid, Arc::new(plugin));

    let references_before_bind = count_plugin_references(
        &plugin_ref,
        Some(plugin_uuid),
        tenant_id,
        state.store.upstreams(),
        state.store.routes(),
    );
    assert_eq!(references_before_bind.count(), 0);

    // Binding (simulated -- upstream-management/route-management own the
    // actual write path): once referenced, gc_eligible_at must clear.
    use oagw::model::route::{Route, RouteMatch, RoutePluginsBinding};
    use oagw::model::upstream::Sharing;

    let route_id = Uuid::new_v4();
    let route = Route {
        id: Some(route_id),
        tenant_id,
        tags: Vec::new(),
        upstream_id: Uuid::new_v4(),
        route_match: RouteMatch::default(),
        plugins: Some(RoutePluginsBinding {
            sharing: Sharing::default(),
            items: vec![plugin_ref.clone()],
        }),
        rate_limit: None,
        enabled: true,
        priority: None,
    };
    state.store.routes().insert(route_id, Arc::new(route));

    let references_after_bind = count_plugin_references(
        &plugin_ref,
        Some(plugin_uuid),
        tenant_id,
        state.store.upstreams(),
        state.store.routes(),
    );
    assert_eq!(references_after_bind.count(), 1);
    assert_eq!(
        references_after_bind.routes,
        vec![format!("gts.cf.core.oagw.route.v1~{route_id}")]
    );
    let gc_while_referenced =
        mark_gc_eligibility(Some("irrelevant"), references_after_bind.count());
    assert_eq!(gc_while_referenced, None);

    // Unbinding: reference count returns to zero -> gc_eligible_at is set again.
    state.store.routes().remove(&route_id);
    let references_after_unbind = count_plugin_references(
        &plugin_ref,
        Some(plugin_uuid),
        tenant_id,
        state.store.upstreams(),
        state.store.routes(),
    );
    assert_eq!(references_after_unbind.count(), 0);
    let gc_after_unbind = mark_gc_eligibility(None, references_after_unbind.count());
    assert!(gc_after_unbind.is_some());
}

/// `cpt-cf-oagw-algo-plugin-gc-mark-sweep` step `inst-gc-set-eligible`
/// documents the GC TTL default as "30 days" -- the internal unit tests in
/// `src/model/plugin/lifecycle.rs` exercise `mark_gc_eligibility`'s
/// none/some transitions and the RFC 3339 formatter's output for two known
/// instants, but none of them pins the *magnitude* of the default TTL that a
/// freshly-unreferenced plugin's `gc_eligible_at` is set to. This computes
/// the day-count independently (a standard Julian Day Number conversion,
/// not a copy of `civil_from_days`/`format_rfc3339_utc` from the
/// implementation) so the assertion is a genuine cross-check rather than a
/// tautology.
#[test]
fn newly_unreferenced_plugin_gc_eligible_at_is_thirty_days_out() {
    fn julian_day_number(year: i64, month: i64, day: i64) -> i64 {
        let a = (14 - month).div_euclid(12);
        let y = year + 4800 - a;
        let m = month + 12 * a - 3;
        day + (153 * m + 2).div_euclid(5) + 365 * y + y.div_euclid(4) - y.div_euclid(100)
            + y.div_euclid(400)
            - 32045
    }

    fn parse_rfc3339_date(value: &str) -> (i64, i64, i64) {
        let date_part = value.split('T').next().unwrap();
        let mut parts = date_part.split('-');
        let year: i64 = parts.next().unwrap().parse().unwrap();
        let month: i64 = parts.next().unwrap().parse().unwrap();
        let day: i64 = parts.next().unwrap().parse().unwrap();
        (year, month, day)
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let today_epoch_days = i64::try_from(now.as_secs() / 86_400).unwrap();
    // Julian Day Number of the Unix epoch (1970-01-01).
    let jdn_epoch = julian_day_number(1970, 1, 1);
    let today_jdn = jdn_epoch + today_epoch_days;

    let gc_eligible_at = mark_gc_eligibility(None, 0).unwrap();
    let (year, month, day) = parse_rfc3339_date(&gc_eligible_at);
    let gc_jdn = julian_day_number(year, month, day);

    let days_out = gc_jdn - today_jdn;
    assert!(
        (29..=31).contains(&days_out),
        "expected gc_eligible_at ~30 days out, got {days_out} days ({gc_eligible_at})"
    );
}

/// A plugin resolved for one tenant must never be visible to another
/// tenant's resolution/lookup, and reference counting must never leak
/// across tenants either.
#[test]
fn tenant_scoping_holds_across_resolve_lookup_and_ref_count() {
    let owner_tenant = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();
    let state = OagwState::new(OagwConfig::default());
    let plugin_uuid = Uuid::new_v4();
    let plugin = sample_plugin(plugin_uuid, owner_tenant, PluginType::Auth, "owner-only");
    state.store.plugins().insert(plugin_uuid, Arc::new(plugin));

    assert!(
        lookup_plugin_for_management(
            &plugin_uuid.to_string(),
            other_tenant,
            state.store.plugins()
        )
        .is_err()
    );
    assert!(
        resolve_plugin_ref(
            &plugin_uuid.to_string(),
            None,
            other_tenant,
            state.store.plugins()
        )
        .is_err()
    );

    use oagw::model::upstream::{ServerConfig, Upstream};
    let plugin_ref = plugin_gts_ref(PluginType::Auth, plugin_uuid);
    let other_upstream_id = Uuid::new_v4();
    let other_upstream = Upstream {
        id: Some(other_upstream_id),
        enabled: true,
        alias: None,
        tags: Vec::new(),
        server: ServerConfig { endpoints: vec![] },
        protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
        auth: Some(oagw::model::upstream::AuthConfig {
            auth_type: Some(plugin_ref.clone()),
            sharing: oagw::model::upstream::Sharing::default(),
            config: serde_json::Value::Null,
        }),
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tenant_id: other_tenant,
    };
    state
        .store
        .upstreams()
        .insert(other_upstream_id, Arc::new(other_upstream));

    let references = count_plugin_references(
        &plugin_ref,
        Some(plugin_uuid),
        owner_tenant,
        state.store.upstreams(),
        state.store.routes(),
    );
    assert_eq!(
        references.count(),
        0,
        "another tenant's binding must not be counted"
    );
}
