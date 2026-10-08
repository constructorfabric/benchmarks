//! GTS re-exports from `toolkit-gts`.
//!
//! Platform base GTS types (plugin base, permission base, and future
//! role/grant/binding) live in the dedicated `toolkit-gts` crate alongside
//! the link-time inventory machinery. This gear re-exports them for
//! convenience so existing consumers can continue writing
//! `use toolkit::gts::PluginV1;` without an extra dependency.
//!
//! A deprecated type alias is provided for the former `BaseToolkitPluginV1`
//! name to preserve backward compatibility of the published `cf-toolkit`
//! crate for external consumers. In-repo callsites have been migrated to
//! `PluginV1`.

pub use toolkit_gts::{
    AuthzPermissionV1, InventoryInstance, InventoryTypeSchema, PluginV1, all_inventory_instances,
    all_inventory_type_schemas,
};

/// Deprecated alias — `BaseToolkitPluginV1` was renamed to [`PluginV1`].
///
/// The "Base" prefix was redundant with the type's role (every
/// `struct_to_gts_schema(base = true)` struct is a base type); "Toolkit" was
/// then dropped since the crate path (`toolkit::gts::`) already carries that
/// context. Callsites in this workspace have all been migrated. This alias
/// is retained to keep the published crate's surface backward-compatible
/// for external consumers.
#[deprecated(since = "0.7.0", note = "Renamed to `PluginV1`")]
pub type BaseToolkitPluginV1<P> = PluginV1<P>;
