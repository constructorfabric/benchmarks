//! Plugin identifier resolution
//! (`cpt-cf-oagw-algo-plugin-system-identifier-resolution`) — the pure half.
//!
//! The algorithm's *policy* lives in the domain because it is a documented
//! algorithm and needs no registry; the *lookup* against the in-process
//! registries and the tenant-scoped plugin repository is the
//! `infra::plugin::resolution` half that consumes these helpers.
//!
//! A plugin reference is always a GTS identifier of the shape
//! `gts.cf.core.oagw.{type}_plugin.v1~{instance}`. The instance part after the
//! `~` separator is either a UUID — a custom plugin persisted in
//! `oagw_plugin` — or a name segment resolved by the in-process registry.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{CATALOG_ONLY_PLUGIN_IDS, PLUGIN_BASE_TYPES};

/// The instance part of a plugin GTS identifier, after the `~` separator.
///
/// `inst-ps-res-1`/`-2`: the instance is parsed first and classified second —
/// a UUID denotes a persisted custom plugin, anything else denotes a named
/// plugin the in-process registry resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginInstance {
    /// A custom plugin persisted in the tenant-scoped plugin repository.
    Uuid(Uuid),
    /// A named plugin resolved by the in-process registry.
    Named(String),
}

/// Parse the instance part of a plugin GTS identifier.
///
/// A reference that carries no `~` separator has no instance part and
/// classifies as a bare name, which the registry lookup then rejects.
#[must_use]
pub fn parse_instance(reference: &str) -> PluginInstance {
    let instance = reference.rsplit('~').next().unwrap_or(reference);
    match Uuid::parse_str(instance) {
        Ok(uuid) => PluginInstance::Uuid(uuid),
        Err(_) => PluginInstance::Named(instance.to_owned()),
    }
}

impl PluginInstance {
    /// The UUID of a UUID-backed instance, or `None` for a named one.
    #[must_use]
    pub const fn uuid(&self) -> Option<Uuid> {
        match self {
            Self::Uuid(uuid) => Some(*uuid),
            Self::Named(_) => None,
        }
    }
}

/// The plugin base type a reference names, or `None` when the reference is not
/// a plugin GTS identifier at all.
#[must_use]
pub fn plugin_base_type_of(reference: &str) -> Option<&'static str> {
    PLUGIN_BASE_TYPES
        .iter()
        .copied()
        .find(|base| reference.starts_with(base))
}

/// Whether the reference names one of the six catalog-only identifiers, which
/// the types-registry catalogs but no plugin registry resolves
/// (`inst-ps-res-7`/`-8`).
#[must_use]
pub fn is_catalog_only(reference: &str) -> bool {
    CATALOG_ONLY_PLUGIN_IDS.contains(&reference)
}

/// Whether the reference names one of the six built-in plugins.
#[must_use]
pub fn is_builtin(reference: &str) -> bool {
    crate::domain::gts_helpers::BUILTIN_PLUGIN_IDS.contains(&reference)
}

/// `inst-ps-bind-7`/`-8`: a stored `plugin_uuid` must agree with the UUID the
/// `plugin_ref` carries, so a disagreement is rejected rather than persisted.
///
/// # Errors
///
/// Returns a validation error naming `plugins.items` when the pair disagrees.
pub fn validate_ref_uuid_agreement(
    plugin_ref: &str,
    plugin_uuid: Option<Uuid>,
) -> Result<(), DomainError> {
    let Some(uuid) = plugin_uuid else {
        return Ok(());
    };
    let agrees = matches!(parse_instance(plugin_ref), PluginInstance::Uuid(parsed) if parsed == uuid);
    if agrees {
        return Ok(());
    }
    Err(DomainError::ValidationError {
        detail: format!(
            "field `plugins.items` rejected: plugin reference `{plugin_ref}` carries a `plugin_uuid` that disagrees with it"
        ),
        path: Some("plugins.items".to_owned()),
        trace_id: None,
    })
}

/// `inst-ps-bind-5`/`-6`: a UUID-backed reference whose record's plugin type
/// disagrees with the base type the reference names is unresolvable.
///
/// # Errors
///
/// Returns a validation error naming `plugins.items` on a type disagreement.
pub fn validate_record_type(plugin_type: &str, reference: &str) -> Result<(), DomainError> {
    let expected = plugin_base_type_of(reference);
    let matches = match expected {
        Some(expected) => plugin_type == expected,
        // A bare named reference (`apikey`) carries no base type to disagree
        // with; the registry lookup decides the type.
        None => true,
    };
    if matches {
        return Ok(());
    }
    Err(DomainError::ValidationError {
        detail: format!(
            "field `plugins.items` rejected: plugin reference `{reference}` resolves to a record of a different plugin type"
        ),
        path: Some("plugins.items".to_owned()),
        trace_id: None,
    })
}
