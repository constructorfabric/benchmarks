//! Plugin reference resolution and binding validation
//! (`cpt-cf-oagw-algo-plugin-identifier-resolve`, `cpt-cf-oagw-algo-plugin-binding-validate`).
//!
//! The classification of an identifier is pure ([`PluginIdentifier::classify`]);
//! resolving it against the *stored* definitions is not, because a UUID-backed
//! reference names a row of the calling tenant. This module therefore takes the
//! repository contract the store implements and nothing else: the named
//! resolution needs no store at all, because the resolvable name set of each
//! plugin type is fixed in [`crate::domain::plugin`] and the builtin registries
//! of `infra/plugin/` are built from exactly that set
//! (`inst-pcrg-08`), so classifying a name and resolving it through a registry
//! agree by construction.
//!
//! Nothing here resolves a plugin into an *executable* instance: that is the
//! data plane of entry 2.5. This module only decides whether a reference may be
//! written into a binding row or into an upstream's scalar `auth` columns.

use uuid::Uuid;

use crate::domain::model::{AuthConfig, PluginBinding};
use crate::domain::plugin::{
    PluginBase, PluginClass, PluginIdentifier, PluginInstance, PluginType, CHAIN_PLUGIN_BASE,
};
use crate::domain::repo::PluginRepository;

/// The slot a plugin reference is used in.
///
/// The slot fixes the plugin types the reference may carry and the field name a
/// rejection names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSlot {
    /// A `plugins.items[]` entry of an upstream or a route: guard and transform
    /// plugins only, the `auth` plugin living in the upstream's scalar `auth`
    /// field (`inst-pbnd-03`).
    Chain,
    /// The scalar `auth` field of an upstream: auth plugins only.
    Auth,
}

impl PluginSlot {
    /// The plugin types the slot accepts.
    #[must_use]
    pub const fn accepted_types(self) -> &'static [PluginType] {
        match self {
            Self::Chain => &[PluginType::Guard, PluginType::Transform],
            Self::Auth => &[PluginType::Auth],
        }
    }

    /// The body field a rejection of this slot names.
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::Chain => "plugins.items",
            Self::Auth => "auth.type",
        }
    }
}

/// What a plugin reference resolves to.
///
/// A named plugin resolves through the name set of its type and has no stored
/// row; a custom plugin resolves to the stored definition's own plugin type,
/// which the reference's base type must agree with (`inst-pbnd-04`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginResolution {
    /// A builtin named plugin: registered, resolvable, never stored.
    Named {
        /// The plugin type the reference's base names.
        plugin_type: PluginType,
        /// The registry key of the plugin.
        name: String,
    },
    /// A UUID-backed custom plugin with the stored definition's plugin type.
    Custom {
        /// The plugin type of the stored definition.
        plugin_type: PluginType,
        /// The instance UUID of the reference.
        uuid: Uuid,
    },
}

impl PluginResolution {
    /// The plugin type of the resolved plugin.
    #[must_use]
    pub const fn plugin_type(&self) -> PluginType {
        match self {
            Self::Named { plugin_type, .. } | Self::Custom { plugin_type, .. } => *plugin_type,
        }
    }
}

/// Resolve one plugin reference in one slot.
///
/// A reference that cannot be used in the slot is a `400` naming the slot's
/// field and the offending value (`inst-pidr-02`, `inst-pidr-10`).
///
/// # Errors
///
/// Returns the mapped `400` of an unresolvable reference, a catalog-only
/// identifier, a base type outside the slot's set and a stored definition whose
/// `plugin_type` does not match the reference's base type.
pub fn resolve_plugin<P: PluginRepository + ?Sized>(
    repo: &P,
    tenant_id: Uuid,
    raw: &str,
    slot: PluginSlot,
) -> Result<PluginResolution, ResolutionError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-02
    // A value that does not parse, a base type that is none of the three plugin
    // types and a base type the slot does not carry are all the same `400`.
    let identifier = PluginIdentifier::parse(raw).ok_or_else(|| ResolutionError {
        field: slot.field().to_owned(),
        detail: format!("`{raw}` is not a plugin identifier"),
    })?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-02

    match (&identifier.base, &identifier.instance) {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-12
        // A bare UUID carries no base type of its own, so the stored definition
        // supplies the type the slot is checked against.
        (PluginBase::Bare, PluginInstance::Uuid(uuid)) => resolve_custom(
            repo,
            tenant_id,
            *uuid,
            None,
            slot,
            identifier.raw.clone(),
        ),
        // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-12
        (PluginBase::Typed(base_type), PluginInstance::Uuid(uuid)) => resolve_custom(
            repo,
            tenant_id,
            *uuid,
            Some(*base_type),
            slot,
            identifier.raw.clone(),
        ),
        (_, PluginInstance::Uuid(uuid)) => {
            // A type-agnostic chain reference (`gts...plugin.v1~{uuid}`) is the
            // wire form entry 2.2 already stores for a `plugins.items[]` entry.
            // It names no plugin type of its own, so the stored row — when one
            // exists — supplies it; when no row exists the reference is kept
            // bindable, because the form declares no type to check the row
            // against and the shipped schemas allow it (the recorded deviation
            // of section 1.2 on the binding row shape).
            match repo.find_plugin_by_uuid(tenant_id, *uuid) {
                Some(record) => {
                    let plugin_type = record.plugin_type;
                    accept(
                        plugin_type,
                        PluginResolution::Custom {
                            plugin_type,
                            uuid: *uuid,
                        },
                        slot,
                        &identifier.raw,
                    )
                }
                None => {
                    let undeclared = *slot
                        .accepted_types()
                        .first()
                        .ok_or_else(|| ResolutionError {
                            field: slot.field().to_owned(),
                            detail: format!("`{}` accepts no plugin type", slot.field()),
                        })?;
                    Ok(PluginResolution::Custom {
                        plugin_type: undeclared,
                        uuid: *uuid,
                    })
                }
            }
        }
        (_, PluginInstance::Named(_)) => {
            // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-05
            // The instance part is `cf.core.oagw.{name}.v1`, so the name is
            // classified through the base type's own name set.
            let named = match identifier.classify() {
                // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-06
                // A resolvable name resolves through the registry of its base
                // type, whose identifier set is the one `domain::plugin` fixes.
                PluginClass::Named(plugin_type, name) => (plugin_type, name),
                // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-06
                // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-07
                // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-08
                // A catalog-only name is registered with no backing
                // implementation: it resolves through no registry and binds
                // through no slot, as `basic` or `bearer` as `auth.plugin_type`.
                PluginClass::CatalogOnly(plugin_type, name) => {
                    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-05
                    return Err(ResolutionError {
                        field: slot.field().to_owned(),
                        detail: catalog_only_detail(&plugin_type, &name),
                    });
                    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-05
                }
                // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-08
                // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-07
                // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-09
                // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-10
                // Any other instance is an unknown plugin, named in the failure.
                PluginClass::Custom(_) | PluginClass::Unknown => {
                    return Err(ResolutionError {
                        field: slot.field().to_owned(),
                        detail: format!("`{raw}` is an unknown plugin"),
                    });
                }
                // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-10
                // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-09
            };
            // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-05
            // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-11
            // The classification is returned with the extracted instance part:
            // the binding validator stores it as `plugin_ref` and, for a
            // UUID-backed plugin, as the matching `plugin_uuid`.
            accept(named.0, PluginResolution::Named {
                plugin_type: named.0,
                name: named.1,
            }, slot, &identifier.raw)
            // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-11
        }
    }
}

/// Resolve a UUID-backed reference against the stored definition.
fn resolve_custom<P: PluginRepository + ?Sized>(
    repo: &P,
    tenant_id: Uuid,
    uuid: Uuid,
    base: Option<PluginType>,
    slot: PluginSlot,
    raw: String,
) -> Result<PluginResolution, ResolutionError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-04
    // The referenced plugin must resolve: a UUID-backed one through a stored
    // definition of the calling tenant, whose `plugin_type` matches the base
    // type of the reference.
    let Some(record) = repo.find_plugin_by_uuid(tenant_id, uuid) else {
        return Err(ResolutionError {
            field: slot.field().to_owned(),
            detail: format!("`{raw}` does not resolve to a plugin definition of this tenant"),
        });
    };
    let plugin_type = record.plugin_type;
    if let Some(base_type) = base
        && base_type != plugin_type
    {
        return Err(ResolutionError {
            field: slot.field().to_owned(),
            detail: format!(
                "`{raw}` names a {} definition, not a {} plugin",
                plugin_type.as_str(),
                base_type.as_str()
            ),
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-04
    accept(
        plugin_type,
        PluginResolution::Custom { plugin_type, uuid },
        slot,
        &raw,
    )
}

/// Reject a resolution whose plugin type the slot does not accept.
fn accept(
    plugin_type: PluginType,
    resolution: PluginResolution,
    slot: PluginSlot,
    raw: &str,
) -> Result<PluginResolution, ResolutionError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-03
    // The slot decides the plugin types the reference may carry: a chain slot
    // rejects an `auth_plugin` reference and the `auth` slot rejects a guard or
    // transform one.
    if slot.accepted_types().contains(&plugin_type) {
        return Ok(resolution);
    }
    Err(ResolutionError {
        field: slot.field().to_owned(),
        detail: format!(
            "`{raw}` names a {} plugin, which the {} slot does not carry",
            plugin_type.as_str(),
            slot.field()
        ),
    })
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-03
}

/// The failure detail of a catalog-only identifier in the `auth` slot.
fn catalog_only_detail(plugin_type: &PluginType, name: &str) -> String {
    match plugin_type {
        // The reserved auth identifiers are the DESIGN's `unknown auth plugin`
        // failure (`inst-pbnd-09`).
        PluginType::Auth => format!("`{name}` is an unknown auth plugin"),
        PluginType::Guard | PluginType::Transform => format!(
            "`{CHAIN_PLUGIN_BASE}`-catalogued identifier `{name}` is not bindable"
        ),
    }
}

/// A rejected plugin reference.
///
/// Carries the body field the caller names and the reason, so the write path of
/// entry 2.2 returns it to the mapping layer as the `400` problem body
/// (`inst-pbnd-13`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionError {
    /// The body field the reference was read from.
    pub field: String,
    /// Occurrence-specific explanation.
    pub detail: String,
}

impl From<ResolutionError> for crate::domain::error::DomainError {
    fn from(error: ResolutionError) -> Self {
        Self::ValidationError {
            detail: format!("{}: {}", error.field, error.detail),
        }
    }
}

/// Validate the binding rows of an upstream or route write
/// (`cpt-cf-oagw-algo-plugin-binding-validate`).
///
/// Every `plugin_ref` is classified before any row is written (`inst-pbnd-01`),
/// the scalar `auth` reference is classified the same way (`inst-pbnd-08`) and
/// the positions are checked to be contiguous from `0` with no gap and no
/// duplicate (`inst-pbnd-07`). The declared `config` of a binding is carried
/// verbatim and is not checked against the referenced plugin's `config_schema`
/// (`inst-pbnd-10`).
///
/// # Errors
///
/// Returns the mapped `400` of the first rejected reference, naming its
/// position, with the store left untouched (`inst-pbnd-12`, `inst-pbnd-13`).
pub fn validate_plugin_bindings<P: PluginRepository + ?Sized>(
    repo: &P,
    tenant_id: Uuid,
    chain: &[PluginBinding],
    auth: Option<&AuthConfig>,
) -> Result<(), crate::domain::error::DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-01
    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-02
    for binding in chain {
        let field = format!("plugins.items[{}].plugin_ref", binding.position);
        // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-01
        // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-12
        // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-13
        // The offending position and reason go back to the calling write path of
        // entry 2.2 for the mapping layer, with no row written and the store
        // untouched.
        resolve_plugin(repo, tenant_id, &binding.reference, PluginSlot::Chain).map_err(
            |error| crate::domain::error::DomainError::ValidationError {
                detail: format!("{field}: {}", error.detail),
            },
        )?;
        // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-13
        // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-12
        // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-01
        // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-06
        // `plugin_ref` is always stored and `plugin_uuid` is stored only for a
        // UUID-backed plugin, matching it when present; the store asserts the
        // pairing again inside its own critical section.
        // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-06
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-02
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-01

    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-08
    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-09
    if let Some(config) = auth {
        resolve_plugin(repo, tenant_id, &config.kind, PluginSlot::Auth)
            .map_err(crate::domain::error::DomainError::from)?;
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-09
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-08

    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-07
    assert_positions_contiguous(chain)?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-07

    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-10
    // The `config` value of a binding stays exactly as declared: no check
    // against the referenced plugin's `config_schema` runs here, per the
    // recorded non-goal of section 1.2.
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-10

    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-11
    // The read-time composition of the effective chain — upstream bindings
    // before route bindings, inherited ancestor plugins appended — is entry 2.5's
    // data plane; this algorithm validates the stored rows only.
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-11

    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-14
    // The normalized binding row set is what the caller writes.
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-validate:p1:inst-pbnd-14

    Ok(())
}

/// Reject a binding row set whose positions are not contiguous from `0`.
fn assert_positions_contiguous(chain: &[PluginBinding]) -> Result<(), ResolutionError> {
    for (expected, binding) in chain.iter().enumerate() {
        let expected = u32::try_from(expected).unwrap_or(u32::MAX);
        if binding.position != expected {
            return Err(ResolutionError {
                field: "plugins.items".to_owned(),
                detail: format!(
                    "binding positions must be contiguous from 0 with no gap and no duplicate; \
                     position {} is out of order",
                    binding.position
                ),
            });
        }
    }
    Ok(())
}

/// Resolve a definition a management read or delete addresses
/// (`cpt-cf-oagw-algo-plugin-identifier-resolve`, `inst-plst-05`).
///
/// The lookup is scoped to the calling tenant before anything else, so a
/// foreign definition is indistinguishable from a missing one
/// (`cpt-cf-oagw-dod-plugin-tenant-scoping`). A named instance — a builtin or a
/// catalog-only identifier — has no row and resolves as missing, exactly as the
/// read, source and delete flows fix (`inst-psrc-04`, `inst-pdel-05`).
///
/// # Errors
///
/// Returns a mapped `400` for a value that is not a plugin identifier and for
/// an identifier whose base type does not match the stored `plugin_type`, and
/// the mapped `404` of an identifier the calling tenant does not hold.
pub fn resolve_definition<P: PluginRepository + ?Sized>(
    repo: &P,
    tenant_id: Uuid,
    identifier: &str,
) -> Result<std::sync::Arc<crate::domain::model::Plugin>, crate::domain::error::ManagementError> {
    let parsed = PluginIdentifier::parse(identifier).ok_or_else(|| {
        crate::domain::error::ManagementError::validation(format!(
            "id: `{identifier}` is not a plugin identifier"
        ))
    })?;

    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-03
    // A UUID-backed reference is looked up in the plugin store; a named one has
    // no row and resolves as missing.
    let uuid = match parsed.instance {
        PluginInstance::Uuid(uuid) => uuid,
        PluginInstance::Named(_) => {
            return Err(crate::domain::error::ManagementError::not_found(format!(
                "`{identifier}` does not resolve to a stored plugin definition"
            )));
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-03

    // @cpt-begin:cpt-cf-oagw-dod-plugin-tenant-scoping:p1:inst-full
    // Every definition lookup is scoped to the calling tenant's key space
    // before anything else, so a foreign definition is indistinguishable from
    // a missing one on get, source and delete.
    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-04
    let Some(record) = repo.find_plugin_by_uuid(tenant_id, uuid) else {
        return Err(crate::domain::error::ManagementError::not_found(format!(
            "`{identifier}` does not resolve to a stored plugin definition"
        )));
    };
    // The base type the identifier carries must agree with the stored
    // `plugin_type`: a mismatched type is a `400` on a direct read, while a
    // bare UUID infers the type from the stored row.
    if let PluginBase::Typed(base_type) = parsed.base
        && base_type != record.plugin_type
    {
        return Err(crate::domain::error::ManagementError::validation(format!(
            "id: `{identifier}` names a {} definition, not a {} plugin",
            record.plugin_type.as_str(),
            base_type.as_str()
        )));
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-resolve:p1:inst-pidr-04
    // @cpt-end:cpt-cf-oagw-dod-plugin-tenant-scoping:p1:inst-full

    Ok(record)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::domain::model::Plugin;
    use crate::domain::plugin::{GUARD_PLUGIN_BASE, AUTH_PLUGIN_BASE};

    const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000007aa");

    /// Repository stub over a fixed definition set.
    struct Repo {
        plugins: Vec<Plugin>,
        calls: Mutex<Vec<Uuid>>,
    }

    impl Repo {
        fn with(plugins: Vec<Plugin>) -> Self {
            Self {
                plugins,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl PluginRepository for Repo {
        fn insert_plugin(
            &self,
            _plugin: Plugin,
        ) -> Result<std::sync::Arc<Plugin>, crate::domain::error::ManagementError> {
            unimplemented!("not used by the resolver")
        }

        fn delete_plugin(
            &self,
            _tenant_id: Uuid,
            _id: &str,
        ) -> Result<(), crate::domain::error::ManagementError> {
            unimplemented!("not used by the resolver")
        }

        fn find_plugin(
            &self,
            tenant_id: Uuid,
            id: &str,
        ) -> Option<std::sync::Arc<Plugin>> {
            self.find_plugin_by_uuid(
                tenant_id,
                crate::domain::plugin::definition_uuid(id)?,
            )
        }

        fn find_plugin_by_uuid(
            &self,
            tenant_id: Uuid,
            id: Uuid,
        ) -> Option<std::sync::Arc<Plugin>> {
            self.calls.lock().expect("calls lock").push(id);
            self.plugins
                .iter()
                .find(|plugin| plugin.tenant_id == tenant_id && plugin.uuid() == Some(id))
                .cloned()
                .map(std::sync::Arc::new)
        }

        fn list_plugins(&self, _tenant_id: Uuid) -> Vec<std::sync::Arc<Plugin>> {
            Vec::new()
        }
    }

    fn definition(plugin_type: PluginType, name: &str, tenant_id: Uuid) -> Plugin {
        let uuid = Uuid::new_v4();
        Plugin {
            id: format!("{}~{uuid}", plugin_type.base_identifier()),
            tenant_id,
            plugin_type,
            name: name.to_owned(),
            description: String::new(),
            config_schema: serde_json::Value::Null,
            phases: Vec::new(),
            source_code: String::new(),
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    fn binding(position: u32, reference: &str) -> PluginBinding {
        PluginBinding {
            position,
            plugin_uuid: crate::domain::plugin::definition_uuid(reference),
            reference: reference.to_owned(),
            config: None,
        }
    }

    #[test]
    fn a_named_guard_reference_resolves_through_its_registry() {
        let repo = Repo::with(Vec::new());
        let reference = format!("{GUARD_PLUGIN_BASE}~cf.core.oagw.required_headers.v1");
        let resolution =
            resolve_plugin(&repo, TENANT, &reference, PluginSlot::Chain).expect("resolves");
        assert_eq!(
            resolution,
            PluginResolution::Named {
                plugin_type: PluginType::Guard,
                name: "required_headers".to_owned(),
            }
        );
        // A named plugin has no row, so the store is never consulted.
        assert!(repo.calls.lock().expect("calls lock").is_empty());
    }

    #[test]
    fn a_custom_reference_resolves_through_the_stored_definition() {
        let record = definition(PluginType::Transform, "enrich", TENANT);
        let uuid = record.uuid().expect("uuid instance");
        let repo = Repo::with(vec![record]);
        let reference = format!("{}~{uuid}", PluginType::Transform.base_identifier());
        let resolution =
            resolve_plugin(&repo, TENANT, &reference, PluginSlot::Chain).expect("resolves");
        assert_eq!(
            resolution,
            PluginResolution::Custom {
                plugin_type: PluginType::Transform,
                uuid,
            }
        );
    }

    #[test]
    fn a_custom_reference_of_another_tenant_is_unresolvable() {
        let record = definition(PluginType::Guard, "foreign", uuid::Uuid::new_v4());
        let uuid = record.uuid().expect("uuid instance");
        let repo = Repo::with(vec![record]);
        let reference = format!("{GUARD_PLUGIN_BASE}~{uuid}");
        let error = resolve_plugin(&repo, TENANT, &reference, PluginSlot::Chain)
            .expect_err("foreign definition");
        assert!(error.detail.contains("does not resolve"), "{}", error.detail);
    }

    #[test]
    fn a_base_type_mismatch_with_the_stored_definition_is_rejected() {
        let record = definition(PluginType::Guard, "mislabelled", TENANT);
        let uuid = record.uuid().expect("uuid instance");
        let repo = Repo::with(vec![record]);
        let reference = format!("{}~{uuid}", PluginType::Transform.base_identifier());
        let error = resolve_plugin(&repo, TENANT, &reference, PluginSlot::Chain)
            .expect_err("mismatched type");
        assert!(error.detail.contains("transform"), "{}", error.detail);
    }

    #[test]
    fn an_auth_plugin_reference_is_refused_in_the_chain() {
        let repo = Repo::with(Vec::new());
        let reference = format!("{AUTH_PLUGIN_BASE}~cf.core.oagw.apikey.v1");
        let error = resolve_plugin(&repo, TENANT, &reference, PluginSlot::Chain)
            .expect_err("auth plugin in the chain");
        assert_eq!(error.field, "plugins.items");
    }

    #[test]
    fn a_guard_plugin_reference_is_refused_in_the_auth_slot() {
        let repo = Repo::with(Vec::new());
        let reference = format!("{GUARD_PLUGIN_BASE}~cf.core.oagw.required_headers.v1");
        let error = resolve_plugin(&repo, TENANT, &reference, PluginSlot::Auth)
            .expect_err("guard plugin in the auth slot");
        assert_eq!(error.field, "auth.type");
    }

    #[test]
    fn a_catalog_only_guard_identifier_is_not_bindable() {
        let repo = Repo::with(Vec::new());
        let reference = format!("{GUARD_PLUGIN_BASE}~cf.core.oagw.timeout.v1");
        let error = resolve_plugin(&repo, TENANT, &reference, PluginSlot::Chain)
            .expect_err("catalog-only identifier");
        assert!(error.detail.contains("not bindable"), "{}", error.detail);
    }

    #[test]
    fn basic_as_auth_plugin_type_is_the_unknown_auth_plugin_failure() {
        let repo = Repo::with(Vec::new());
        let reference = format!("{AUTH_PLUGIN_BASE}~cf.core.oagw.basic.v1");
        let error = resolve_plugin(&repo, TENANT, &reference, PluginSlot::Auth)
            .expect_err("catalog-only auth identifier");
        assert!(
            error.detail.contains("unknown auth plugin"),
            "{}",
            error.detail
        );
    }

    #[test]
    fn an_unparsable_reference_is_rejected() {
        let repo = Repo::with(Vec::new());
        let error = resolve_plugin(&repo, TENANT, "not-a-plugin", PluginSlot::Chain)
            .expect_err("unparsable");
        assert!(error.detail.contains("not a plugin identifier"));
    }

    #[test]
    fn the_binding_set_is_refused_when_a_position_is_out_of_order() {
        let repo = Repo::with(Vec::new());
        let chain = vec![
            binding(0, &format!("{GUARD_PLUGIN_BASE}~cf.core.oagw.required_headers.v1")),
            binding(2, &format!("{}~cf.core.oagw.request_id.v1", PluginType::Transform.base_identifier())),
        ];
        let error = validate_plugin_bindings(&repo, TENANT, &chain, None)
            .expect_err("non-contiguous positions");
        assert!(error.detail().contains("contiguous"), "{}", error.detail());
    }

    #[test]
    fn an_auth_reference_is_classified_with_the_chain() {
        let record = definition(PluginType::Auth, "custom-auth", TENANT);
        let uuid = record.uuid().expect("uuid instance");
        let repo = Repo::with(vec![record]);
        let chain = vec![binding(
            0,
            &format!("{}~cf.core.oagw.request_id.v1", PluginType::Transform.base_identifier()),
        )];
        let auth = AuthConfig {
            kind: format!("{}~{uuid}", AUTH_PLUGIN_BASE),
            sharing: crate::domain::model::Sharing::Private,
            config: std::collections::BTreeMap::new(),
        };
        validate_plugin_bindings(&repo, TENANT, &chain, Some(&auth)).expect("valid");
    }

    #[test]
    fn a_config_declared_on_a_binding_is_never_checked() {
        let record = definition(PluginType::Guard, "configless", TENANT);
        let uuid = record.uuid().expect("uuid instance");
        let repo = Repo::with(vec![record]);
        let reference = format!("{GUARD_PLUGIN_BASE}~{uuid}");
        let chain = vec![PluginBinding {
            config: Some(serde_json::json!({ "undeclared": true })),
            ..binding(0, &reference)
        }];
        // The recorded non-goal: `config` is kept as declared, with no check
        // against the referenced plugin's `config_schema`.
        validate_plugin_bindings(&repo, TENANT, &chain, None).expect("config is not validated");
        assert_eq!(
            chain[0].config,
            Some(serde_json::json!({ "undeclared": true }))
        );
    }
}
