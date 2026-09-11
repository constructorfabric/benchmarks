//! Proxy-time plugin identifier resolution
//! (`cpt-cf-oagw-dod-plugin-system-identifier-resolution`): the UUID-versus-
//! named split, the tenant-chain walk, the catalog-only rejection and the
//! proxy-time `PluginNotFound`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    AUTH_PLUGIN_BASE_TYPE, NOOP_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::plugin::identifier::{parse_instance, PluginInstance};
use crate::domain::plugin::PluginKind;
use crate::infra::plugin::resolution::{
    kind_of, resolve_auth, resolve_reference, PluginRegistries, ResolvedBinding, TenantChain,
};
use crate::infra::storage::Storage;
use crate::test_support::FakeCredStore;

const CATALOG_ONLY_REF: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";

fn registries() -> (PluginRegistries, Arc<Storage>) {
    let storage = Arc::new(Storage::new());
    let (_upstreams, _routes, plugins) = storage.repositories();
    let registries = PluginRegistries::with_builtins(
        Arc::new(FakeCredStore),
        crate::config::TokenCacheConfig::default(),
        plugins,
    );
    (registries, storage)
}

/// A custom guard plugin row owned by `tenant`.
fn custom_row(tenant: Uuid, name: &str) -> crate::domain::dto::Plugin {
    crate::domain::dto::Plugin {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: crate::domain::gts_helpers::GUARD_PLUGIN_BASE_TYPE.to_owned(),
        name: name.to_owned(),
        config_schema: Some(serde_json::json!({ "type": "object" })),
        source_code: Some("const reference = 'opaque';".to_owned()),
        last_used_at: None,
        gc_eligible_at: None,
    }
}

/// `inst-ps-res-1`/`-2`: a named built-in reference resolves through the
/// in-process registry of its type, and the resolution carries the plugin kind
/// and the built-in configuration schema.
#[test]
fn a_named_builtin_resolves_through_its_registry() {
    let (registries, _storage) = registries();
    let tenant = Uuid::new_v4();
    let chain = TenantChain::new(vec![tenant]);

    let resolved = resolve_reference(&registries, &chain, REQUIRED_HEADERS_GUARD_PLUGIN_ID)
        .expect("the guard resolves");
    assert_eq!(resolved.kind(), Some(PluginKind::Guard));
    assert!(matches!(resolved.binding, Some(ResolvedBinding::Guard(_))));
    assert!(resolved.config_schema.is_some(), "the built-in carries its schema");

    let resolved = resolve_reference(&registries, &chain, REQUEST_ID_TRANSFORM_PLUGIN_ID)
        .expect("the transform resolves");
    assert_eq!(resolved.kind(), Some(PluginKind::Transform));
    assert!(matches!(resolved.binding, Some(ResolvedBinding::Transform(_))));

    let resolved = resolve_reference(&registries, &chain, NOOP_AUTH_PLUGIN_ID).expect("resolves");
    assert_eq!(resolved.kind(), Some(PluginKind::Auth));
    assert!(matches!(resolved.binding, Some(ResolvedBinding::Auth(_))));
}

/// `inst-ps-res-3`/`-4`: a UUID-backed reference resolves through the
/// tenant-scoped repository and carries the stored `config_schema`.
#[test]
fn a_uuid_backed_reference_resolves_through_the_repository() {
    let (registries, _storage) = registries();
    let tenant = Uuid::new_v4();
    let row = custom_row(tenant, "tenant-guard");
    registries.plugins.create(tenant, row.clone()).expect("stored");

    // The UUID instance is resolved regardless of the base type the
    // reference spells: the record's own type decides.
    let guard_reference = format!(
        "gts.cf.core.oagw.guard_plugin.v1~{}",
        row.id
    );
    let resolved =
        resolve_reference(&registries, &TenantChain::new(vec![tenant]), &guard_reference)
            .expect("the record resolves");
    assert_eq!(resolved.kind(), Some(PluginKind::Guard));
    assert_eq!(
        resolved.config_schema.as_ref().expect("schema")["type"],
        "object",
        "the stored schema is carried"
    );
    assert!(resolved.binding.is_none(), "a custom plugin never executes");
}

/// `inst-ps-res-13`/`-14`: a UUID-backed reference bound by an ancestor
/// resolves against the owning tenant's record through the tenant-chain walk.
#[test]
fn the_tenant_chain_walk_resolves_an_ancestor_bound_record() {
    let (registries, _storage) = registries();
    let owner = Uuid::new_v4();
    let caller = Uuid::new_v4();
    let row = custom_row(owner, "ancestor-guard");
    registries.plugins.create(owner, row.clone()).expect("stored");

    let reference = format!("gts.cf.core.oagw.guard_plugin.v1~{}", row.id);
    let resolved = resolve_reference(
        &registries,
        &TenantChain::new(vec![caller, owner]),
        &reference,
    )
    .expect("the walk reaches the owning tenant");
    assert_eq!(resolved.kind(), Some(PluginKind::Guard));
}

/// `inst-ps-res-13`/`-14`: a record the caller owns directly is found on the
/// first hop, and a record outside the whole chain is not found.
#[test]
fn a_record_outside_the_chain_is_not_found() {
    let (registries, _storage) = registries();
    let owner = Uuid::new_v4();
    let stranger = Uuid::new_v4();
    let row = custom_row(owner, "owned-guard");
    registries.plugins.create(owner, row.clone()).expect("stored");

    let reference = format!("gts.cf.core.oagw.guard_plugin.v1~{}", row.id);
    let error = resolve_reference(&registries, &TenantChain::new(vec![stranger]), &reference)
        .expect_err("the chain does not reach the owner");
    assert!(matches!(error, DomainError::PluginNotFound { .. }), "{error}");
}

/// A record whose type disagrees with the reference's base type is rejected,
/// so a binding cannot be misread as another kind of plugin.
#[test]
fn a_type_mismatch_is_rejected() {
    let (registries, _storage) = registries();
    let tenant = Uuid::new_v4();
    let row = custom_row(tenant, "guard-typed");
    registries.plugins.create(tenant, row.clone()).expect("stored");
    let reference = format!("{}{}", TRANSFORM_BASE, row.id);
    let error = resolve_reference(&registries, &TenantChain::new(vec![tenant]), &reference)
        .expect_err("the types disagree");
    assert!(matches!(error, DomainError::ValidationError { .. }), "{error}");
}

const TRANSFORM_BASE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// `inst-ps-res-5`/`-6`: a catalog-only identifier never resolves at proxy
/// time either.
#[test]
fn a_catalog_only_identifier_never_resolves() {
    let (registries, _storage) = registries();
    let error = resolve_reference(
        &registries,
        &TenantChain::new(vec![Uuid::new_v4()]),
        CATALOG_ONLY_REF,
    )
    .expect_err("the catalog-only identifier is unresolvable");
    assert!(matches!(error, DomainError::PluginNotFound { .. }), "{error}");
}

/// `inst-ps-res-10`/`-11`: a reference nothing matches fails the request with
/// `PluginNotFound` rather than being skipped.
#[test]
fn an_unknown_reference_is_plugin_not_found() {
    let (registries, _storage) = registries();
    for reference in [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.absent.v1",
        "gts.cf.core.oagw.guard_plugin.v1~not-a-uuid",
    ] {
        let error = resolve_reference(
            &registries,
            &TenantChain::new(vec![Uuid::new_v4()]),
            reference,
        )
        .expect_err("unresolvable");
        assert!(
            matches!(&error, DomainError::PluginNotFound { plugin_ref } if plugin_ref == reference),
            "{error}"
        );
    }
}

/// `inst-ps-res-7` .. `-9`: the resolution is typed, so a named reference from
/// one base type never lands in another type's registry.
#[test]
fn a_named_reference_never_resolves_in_another_type() {
    let (registries, _storage) = registries();
    let chain = TenantChain::new(vec![Uuid::new_v4()]);
    // A guard name handed to the transform base type resolves nothing.
    let error = resolve_reference(
        &registries,
        &chain,
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.required_headers.v1",
    )
    .expect_err("wrong type");
    assert!(matches!(error, DomainError::PluginNotFound { .. }), "{error}");
}

/// `resolve_auth` is the `auth`-block half of the same resolution.
#[test]
fn the_auth_block_resolves_through_the_same_path() {
    let (registries, _storage) = registries();
    let chain = TenantChain::new(vec![Uuid::new_v4()]);
    assert!(resolve_auth(&registries, &chain, None).expect("no auth").is_none());
    let resolved = resolve_auth(&registries, &chain, Some(NOOP_AUTH_PLUGIN_ID))
        .expect("the auth plugin resolves")
        .expect("some auth");
    assert_eq!(resolved.kind(), Some(PluginKind::Auth));
    let error = resolve_auth(&registries, &chain, Some(CATALOG_ONLY_REF)).expect_err("unresolvable");
    assert!(matches!(error, DomainError::PluginNotFound { .. }), "{error}");
}

/// The kind classifier reads the base type the plugin type names.
#[test]
fn the_kind_classifier_reads_the_base_type() {
    assert_eq!(kind_of(NOOP_AUTH_PLUGIN_ID), Some(PluginKind::Auth));
    assert_eq!(kind_of(REQUIRED_HEADERS_GUARD_PLUGIN_ID), Some(PluginKind::Guard));
    assert_eq!(kind_of(REQUEST_ID_TRANSFORM_PLUGIN_ID), Some(PluginKind::Transform));
    assert_eq!(kind_of("gts.cf.core.oagw.upstream.v1~x"), None);
    assert_eq!(kind_of("gibberish"), None);
}

/// The tenant chain is ordered base first, which is what makes the walk an
/// ancestor walk.
#[test]
fn the_tenant_chain_keeps_its_order() {
    let base = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let chain = TenantChain::new(vec![base, leaf]);
    assert_eq!(chain.tenants(), [base, leaf]);
    assert!(TenantChain::default().tenants().is_empty());
}

/// The instance parser splits the two forms the resolution branches on.
#[test]
fn the_instance_parser_splits_the_two_forms() {
    let uuid = Uuid::new_v4();
    assert_eq!(
        parse_instance(&format!("{}{}", AUTH_PLUGIN_BASE_TYPE, uuid)),
        PluginInstance::Uuid(uuid)
    );
    assert_eq!(
        parse_instance(NOOP_AUTH_PLUGIN_ID),
        PluginInstance::Named("cf.core.oagw.noop.v1".to_owned())
    );
}
