//! Plugin reference resolution and binding validation — the two algorithms
//! DECOMPOSITION §2.4 names `cpt-cf-oagw-algo-plugin-ref-resolve` and
//! `cpt-cf-oagw-algo-binding-validate`.
//!
//! A parent write that carries a `plugins` sub-object, and an upstream write
//! that carries an `auth` sub-configuration, resolves every reference it names
//! and validates every binding rule before any row is written, and produces
//! the write set the parent's single transaction persists. Resolution follows
//! the DESIGN §3.1 algorithm in its order: the identifier's instance part
//! selects the persisted store for a UUID and the named registry for a name,
//! the resolved plugin's base type must match the identifier's prefix, and a
//! carried `plugin_uuid` must agree with the plugin the reference resolved to.
//!
//! Nothing here resolves a credential: the references a binding carries are
//! checked for their `cred://` shape and carried opaque from there on, which
//! is what keeps a management write free of credential-store calls.

// @cpt-dod:cpt-cf-oagw-dod-binding-model:p1

use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin_contract::{NamedPluginRegistry, PluginFamily, PluginResolveError};
use crate::plugins::credential;
pub use crate::store::{AuthIdentity, BindingWrite};
use crate::store::{OagwStore, PluginBinding};

/// The upper bound one upstream or route chain holds: one auth plugin, bound
/// through the upstream's scalar columns, and never through a binding row.
const MAX_AUTH_PLUGINS: usize = 1;

/// The plugin one reference resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedPlugin {
    /// A persisted custom row: UUID-backed, resolved from `oagw_plugin`.
    Custom {
        /// The family the row's `plugin_type` names.
        family: PluginFamily,
        /// The row's identifier, which is the UUID the instance part named.
        id: Uuid,
    },
    /// A named registry entry: never stored, never garbage-collected.
    Named {
        /// The family the identifier's base type names.
        family: PluginFamily,
        /// The identifier, as submitted.
        identifier: String,
    },
}

impl ResolvedPlugin {
    /// The family the resolved plugin belongs to.
    #[must_use]
    pub const fn family(&self) -> PluginFamily {
        match self {
            Self::Custom { family, .. } | Self::Named { family, .. } => *family,
        }
    }

    /// Whether the resolved plugin has a row in `oagw_plugin`.
    #[must_use]
    pub const fn uuid_backed(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }

    /// The canonical identifier the resolved plugin is persisted with.
    #[must_use]
    pub fn canonical_ref(&self) -> String {
        match self {
            Self::Custom { family, id } => crate::gts::gts_instance(family.base_type(), *id),
            Self::Named { identifier, .. } => identifier.clone(),
        }
    }
}

/// Why one reference is not resolvable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveFailure {
    /// No persisted row matched the identifier, or the row's `plugin_type`
    /// does not match the base type the identifier's prefix names.
    Store {
        /// Whether no row matched at all, as distinct from a row of the wrong
        /// type.
        absent: bool,
    },
    /// The named registry refused the identifier: it is one of the six
    /// catalog-only identifiers, or it is unknown to the registry.
    Registry(PluginResolveError),
    /// The carried `plugin_uuid` does not agree with the plugin the reference
    /// resolved to.
    UuidMismatch,
    /// The reference is not a plugin identifier at all: no plugin base type
    /// prefix names a family for it, so no plugin of any kind answers it.
    Malformed,
}

impl ResolveFailure {
    /// The reason one failed reference is named with, in the validation error
    /// the parent write answers with.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Store { absent: true } => {
                String::from("no plugin row of the calling tenant answers the identifier")
            }
            Self::Store { absent: false } => String::from(
                "the plugin row the identifier answers carries a plugin_type the identifier's base type does not name",
            ),
            Self::Registry(PluginResolveError::Reserved { .. }) => String::from(
                "plugin_type names a catalogue identifier no plugin family backs",
            ),
            Self::Registry(_) => String::from("plugin_type names no resolvable plugin"),
            Self::UuidMismatch => String::from("plugin_uuid does not match the plugin the reference names"),
            Self::Malformed => String::from(
                "the reference is not a plugin identifier, so no plugin base type names a family for it",
            ),
        }
    }
}

/// The parsed parts of one `plugin_ref`.
struct ParsedRef<'a> {
    /// The family the identifier's base type prefix names.
    family: PluginFamily,
    /// The part after the `~` separator.
    instance: &'a str,
}

/// Parses one reference into its base type and its instance part.
///
/// A reference that names no plugin base type is refused: the type match the
/// algorithm requires compares the resolved plugin's family against the base
/// type the identifier declares, and a bare UUID declares none.
fn parse_ref(reference: &str) -> Option<ParsedRef<'_>> {
    let (family, instance) = PluginFamily::parse_identifier(reference)?;
    let family = family?;
    Some(ParsedRef { family, instance })
}

/// Resolves one reference through the persisted store and the named registry.
///
/// # Errors
///
/// Returns the reason the reference is not resolvable, never echoing the
/// reference value itself.
#[allow(clippy::result_large_err)]
pub fn resolve(
    store: &OagwStore,
    tenant_id: Uuid,
    registry: &NamedPluginRegistry,
    reference: &str,
    carried_uuid: Option<Uuid>,
) -> Result<ResolvedPlugin, ResolveFailure> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-parse
    // Parse the GTS identifier to extract the instance part after the `~`
    // separator.
    let parsed = parse_ref(reference).ok_or(ResolveFailure::Malformed)?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-parse

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuid-if
    let resolved = if Uuid::parse_str(parsed.instance).is_ok() {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-store
        // The instance part parses as a UUID, so the plugin is resolved from
        // the persisted store, scoped to the tenant the binding's parent row
        // belongs to.
        let row = store.get_plugin(tenant_id, parsed.instance.parse::<Uuid>().unwrap_or_default());
        // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-store
        // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-store-fail-if
        let row = match row {
            None => {
                // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-store-fail-return
                // No row matched: the reference names nothing the tenant owns.
                return Err(ResolveFailure::Store { absent: true });
                // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-store-fail-return
            }
            // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-store-fail-if
            Some(row) => row,
        };
        if PluginFamily::from_type_literal(&row.plugin.plugin_type) != Some(parsed.family) {
            // The row's `plugin_type` does not match the base type the
            // identifier's prefix names, so the identifier answers no plugin
            // of the kind it declares.
            return Err(ResolveFailure::Store { absent: false });
        }
        ResolvedPlugin::Custom {
            family: parsed.family,
            id: row.plugin.id,
        }
    } else {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named-else
        // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named
        // The instance part is a name, so the identifier is resolved through
        // the named registry, whose lookup fails for a catalog-only identifier
        // and for an unknown one alike.
        // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named-fail-if
        // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named-fail-return
        registry.resolve(reference).map_err(ResolveFailure::Registry)?;
        // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named-fail-return
        // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named-fail-if
        // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named
        ResolvedPlugin::Named {
            family: parsed.family,
            identifier: String::from(reference),
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-named-else
    };
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuid-if

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuidcheck-if
    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuidcheck-fail-if
    if let Some(carried) = carried_uuid {
        // A carried `plugin_uuid` must name the same plugin the reference
        // resolved to, and a named plugin carries none at all.
        let agrees = match &resolved {
            ResolvedPlugin::Custom { id, .. } => *id == carried,
            ResolvedPlugin::Named { .. } => false,
        };
        if !agrees {
            // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuidcheck-fail-return
            return Err(ResolveFailure::UuidMismatch);
            // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuidcheck-fail-return
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuidcheck-fail-if
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-uuidcheck-if

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-return
    // The resolved plugin carries whether it is UUID-backed, so the caller
    // stores the UUID only when the plugin has a row.
    Ok(resolved)
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-resolve:p1:inst-ref-return
}


/// The auth plugin identity one upstream body's `auth` sub-configuration
/// carries, as the body wrote it.
struct SubmittedAuth {
    /// The `auth.type` member.
    plugin_type: String,
}

/// The submitted `plugins` items of one parent body, as the body carried them.
struct SubmittedBinding {
    /// The position the item carried, when it carried one.
    position: Option<u32>,
    /// The `plugin_ref` the item named.
    plugin_ref: String,
    /// The `plugin_uuid` the item carried, when it carried one.
    plugin_uuid: Option<Uuid>,
    /// The configuration the item carried.
    config: Value,
}

/// Validates one parent body's bindings and builds the write set.
///
/// The checks are the DESIGN §3.6 key invariants and the DESIGN §3.1
/// resolution algorithm: contiguous positions from 0, every reference
/// resolved, the type match, the `plugin_uuid` match, at most one auth plugin
/// bound through the scalar columns and none through a binding row, the
/// `cred://` shape of every credential reference, and the tenancy of every
/// custom reference. Every failure is accumulated into one validation error,
/// so a caller is not made to retry once per defect.
///
/// # Errors
///
/// Returns one gateway validation error naming every failing item with its
/// position and the reason.
#[allow(clippy::result_large_err)]
pub fn validate(
    store: &OagwStore,
    tenant_id: Uuid,
    registry: &NamedPluginRegistry,
    body: &Value,
    is_upstream: bool,
    marked_at: u64,
) -> Result<BindingWrite, DomainError> {
    let mut defects = Defects::default();

    let submitted = submitted_items(body, is_upstream, &mut defects);

    // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-positions
    // The submitted positions are read in the order the body carries them, and
    // an omitted `position` defaults to the item's index, so the ADR example —
    // which carries none — binds at position 0. The contiguous set from 0 in
    // the submitted order is then one check: the effective position of every
    // item is its index.
    for (index, item) in submitted.iter().enumerate() {
        // The effective position of every item is its index, so an item that
        // carries a position other than its own breaks the contiguous set.
        let carried = item.position.map_or(index, |position| {
            usize::try_from(position).unwrap_or(usize::MAX)
        });
        if carried != index {
            let carried = item.position.map_or_else(String::new, |position| format!("{position} "));
            defects.add(
                index,
                &format!("position {}is not the submitted order", carried),
            );
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-positions

    // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-loop
    let mut bindings = Vec::with_capacity(submitted.len());
    for (index, item) in submitted.iter().enumerate() {
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-resolve
        // Every reference is resolved, and the resolved type is checked
        // against the family the slot carries: a binding row is a guard or a
        // transform slot, and the auth slot is the upstream's scalar columns.
        let resolved = resolve(
            store,
            tenant_id,
            registry,
            &item.plugin_ref,
            item.plugin_uuid,
        );
        // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-resolve
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-item-fail-if
        match resolved {
            Ok(resolved) if resolved.family() != PluginFamily::Auth => {
                // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-item-fail
                // The stored row carries the reference always and the UUID
                // only because the plugin is UUID-backed.
                bindings.push(PluginBinding {
                    position: index as u32,
                    plugin_ref: resolved.canonical_ref(),
                    plugin_uuid: resolved.uuid_backed().then(|| match resolved {
                        ResolvedPlugin::Custom { id, .. } => id,
                        ResolvedPlugin::Named { .. } => Uuid::nil(),
                    }),
                    config: item.config.clone(),
                });
                // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-item-fail
            }
            Ok(_) => defects.add(
                index,
                "the item names an auth plugin, which is bound through the upstream's auth sub-configuration and never through a binding row",
            ),
            Err(failure) => defects.add(index, &failure.reason()),
        }
        // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-item-fail-if
    }
    // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-loop

    let mut auth = None;
    // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-upstream-if
    if is_upstream {
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-auth
        // The `auth` sub-configuration names the one auth plugin the upstream
        // binds, through its scalar columns; a route body that carries one is
        // a schema failure the parent validation already answered.
        auth = validate_auth(store, tenant_id, registry, body, &mut defects);
        // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-auth
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-credshape
        // Every credential reference the auth plugin configuration carries is
        // checked for its `cred://` shape and resolved by nobody here.
        check_credential_references(body, &mut defects);
        // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-credshape
    }
    // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-upstream-if

    // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-fail-if
    if defects.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-fail-else
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-write-set
        // The write set is the full replacement of the parent's binding rows,
        // which is what makes a body that omits the `plugins` sub-object clear
        // them.
        // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-return
        return Ok(BindingWrite {
            bindings,
            auth,
            marked_at,
        });
        // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-return
        // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-write-set
    }
    // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-fail-else
    // @cpt-begin:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-fail-return
    Err(defects.into_error())
    // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-fail-return
    // @cpt-end:cpt-cf-oagw-algo-binding-validate:p1:inst-bindv-fail-if
}

/// Reads the submitted `plugins` items and the auth identity off the body.
fn submitted_items(body: &Value, is_upstream: bool, defects: &mut Defects) -> Vec<SubmittedBinding> {
    let mut submitted = Vec::new();
    let Some(items) = body
        .get("plugins")
        .and_then(|plugins| plugins.get("items"))
        .and_then(Value::as_array)
    else {
        return submitted;
    };
    for (index, item) in items.iter().enumerate() {
        match binding_item(item) {
            Ok(item) => submitted.push(item),
            Err(reason) => defects.add(index, &reason),
        }
    }
    if !is_upstream && body.get("auth").is_some() {
        // A route carries no `auth` sub-configuration at all; the schema root
        // refuses the member, and the check here is the record of that rule.
        defects.take_auth();
    }
    submitted
}

/// Reads one submitted item into the binding it validates to.
fn binding_item(item: &Value) -> Result<SubmittedBinding, String> {
    let (reference, carried_uuid, config, position) = match item {
        Value::String(identifier) => (identifier.clone(), None, Value::Object(Default::default()), None),
        Value::Object(fields) => {
            let reference = fields
                .get("plugin_ref")
                .and_then(Value::as_str)
                .ok_or_else(|| String::from("the item carries no plugin_ref to identify the plugin by"))?;
            let carried_uuid = match fields.get("plugin_uuid") {
                None => None,
                Some(value) => match value.as_str().map(Uuid::parse_str) {
                    Some(Ok(uuid)) => Some(uuid),
                    _ => {
                        return Err(String::from("plugin_uuid"));
                    }
                },
            };
            let config = fields.get("config").cloned().unwrap_or_else(|| {
                Value::Object(serde_json::Map::new())
            });
            if !config.is_object() {
                return Err(String::from("config"));
            }
            let position = match fields.get("position") {
                None => None,
                Some(value) => match value.as_u64() {
                    Some(position) => Some(u32::try_from(position).map_err(|_| String::from("position"))?),
                    None => return Err(String::from("position")),
                },
            };
            (String::from(reference), carried_uuid, config, position)
        }
        other => {
            return Err(format!(
                "the item is neither an identifier nor a plugin binding object: {other}"
            ))
        }
    };
    Ok(SubmittedBinding {
        position,
        plugin_ref: reference,
        plugin_uuid: carried_uuid,
        config,
    })
}

/// Validates the `auth` sub-configuration of one upstream body.
fn validate_auth(
    store: &OagwStore,
    tenant_id: Uuid,
    registry: &NamedPluginRegistry,
    body: &Value,
    defects: &mut Defects,
) -> Option<AuthIdentity> {
    let auth = body.get("auth")?;
    // A sub-configuration that names no plugin binds no auth plugin, which the
    // shipped schema admits by declaring the member optional: the sharing mode
    // alone is a legal body, and it writes no identity column.
    let plugin_type = auth.get("type").and_then(Value::as_str)?;
    let submitted = SubmittedAuth {
        plugin_type: String::from(plugin_type),
    };
    // The auth slot is one per upstream and carries an auth identifier only.
    let resolved = resolve(
        store,
        tenant_id,
        registry,
        &submitted.plugin_type,
        None,
    );
    match resolved {
        Ok(resolved) if resolved.family() == PluginFamily::Auth => Some(AuthIdentity {
            plugin_ref: resolved.canonical_ref(),
            plugin_uuid: resolved.uuid_backed().then(|| match resolved {
                ResolvedPlugin::Custom { id, .. } => id,
                ResolvedPlugin::Named { .. } => Uuid::nil(),
            }),
        }),
        Ok(resolved) => {
            defects.add(
                MAX_AUTH_PLUGINS,
                &format!(
                    "auth.type names a {} plugin, which is not an auth plugin",
                    resolved.family().as_str()
                ),
            );
            None
        }
        Err(failure) => {
            defects.add(MAX_AUTH_PLUGINS, &failure.reason());
            None
        }
    }
}

/// Checks every credential reference the `auth` sub-configuration carries for
/// its `cred://` shape.
///
/// The reference members are the `auth.secret_ref` the shipped schema names
/// and every `*_ref` member of the plugin configuration, which is the shape
/// the built-in auth plugins read their references through. None is resolved,
/// and no reference value is echoed into the answer.
fn check_credential_references(body: &Value, defects: &mut Defects) {
    let Some(auth) = body.get("auth") else {
        return;
    };
    if let Some(reference) = auth.get("secret_ref").and_then(Value::as_str) {
        check_shape(reference, "auth.secret_ref", defects);
    }
    let Some(config) = auth.get("config").and_then(Value::as_object) else {
        return;
    };
    for (key, value) in config {
        if !key.ends_with("_ref") {
            continue;
        }
        let Some(reference) = value.as_str() else {
            continue;
        };
        check_shape(reference, &format!("auth.config.{key}"), defects);
    }
}

/// Names the member when the reference does not carry the `cred://` shape.
fn check_shape(reference: &str, property: &str, defects: &mut Defects) {
    if !credential::is_credential_reference(reference) {
        defects.add(MAX_AUTH_PLUGINS, property);
    }
}

/// Accumulated failing items of one binding validation.
#[derive(Debug, Default)]
struct Defects {
    /// One entry per failing item: the position and the reason.
    items: Vec<String>,
    /// Whether the body carried an `auth` sub-configuration where none is
    /// admitted.
    auth: bool,
}

impl Defects {
    /// Adds one failing item, dropping a repeat.
    fn add(&mut self, position: usize, reason: &str) {
        let detail = format!("plugins.items[{position}]: {reason}");
        if !self.items.contains(&detail) {
            self.items.push(detail);
        }
    }

    /// Records that the body carried an `auth` sub-configuration it may not.
    fn take_auth(&mut self) {
        self.auth = true;
    }

    /// Whether every check passed.
    fn is_empty(&self) -> bool {
        self.items.is_empty() && !self.auth
    }

    /// The single validation error naming every failing item.
    fn into_error(self) -> DomainError {
        let mut details = self.items;
        if self.auth {
            details.push(String::from("auth"));
        }
        DomainError::gateway(ErrorKind::ValidationError, details.join(", "))
    }
}
