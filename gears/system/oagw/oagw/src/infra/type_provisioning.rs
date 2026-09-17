//! GTS type provisioning for the OAGW gear (DESIGN §3.2 / §3.4).
//!
//! The gear serves a fixed set of GTS types: the configuration entities it
//! manages and the error types it emits. Declaring them in one place lets the
//! types-registry integration (and any registry-only deployment, DESIGN §4.7)
//! enumerate the gear's contract without OAGW reaching out over the network —
//! provisioning is a *declaration*, the actual registration is driven by the
//! platform.
//!
//! The base-type ids of the managed entities are owned by
//! [`crate::domain::types`], where the domain records use them to build their
//! own GTS instance ids; they are re-exported here so provisioning is the single
//! catalogue of what the gear serves (and so the two lists cannot diverge).

/// GTS base type id of an upstream configuration.
///
/// Re-exported from [`crate::domain::types`], the single declaration site.
pub use crate::domain::types::{
    AUTH_PLUGIN_TYPE_ID, GUARD_PLUGIN_TYPE_ID, ROUTE_TYPE_ID, TRANSFORM_PLUGIN_TYPE_ID,
    UPSTREAM_TYPE_ID,
};

/// GTS base type id of the OAGW error catalogue.
pub const ERROR_TYPE_ID: &str = "gts.cf.core.errors.err.v1~";

/// The GTS type ids the OAGW gear serves.
///
/// Instance identifiers are formed as `{base_type}~{uuid}` (or, for errors,
/// `gts.cf.core.errors.err.v1~cf.oagw.<name>.v1` — see
/// [`crate::error::OagwErrorKind::gts_type_id`]).
#[must_use]
pub const fn served_gts_type_ids() -> [&'static str; 6] {
    [
        UPSTREAM_TYPE_ID,
        ROUTE_TYPE_ID,
        AUTH_PLUGIN_TYPE_ID,
        GUARD_PLUGIN_TYPE_ID,
        TRANSFORM_PLUGIN_TYPE_ID,
        ERROR_TYPE_ID,
    ]
}

/// A provisioning declaration: the set of GTS types a gear instance serves.
///
/// Constructing it has no side effects — it is a value the platform can
/// inspect, persist or forward to the types-registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeProvisioning {
    served_type_ids: Vec<&'static str>,
}

impl TypeProvisioning {
    /// Declare the OAGW type catalogue.
    #[must_use]
    pub fn oagw() -> Self {
        Self {
            served_type_ids: served_gts_type_ids().to_vec(),
        }
    }

    /// The declared GTS base type ids.
    #[must_use]
    pub fn served_type_ids(&self) -> &[&'static str] {
        &self.served_type_ids
    }

    /// The number of declared types.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.served_type_ids.len()
    }

    /// `true` when no type is declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.served_type_ids.is_empty()
    }
}

impl Default for TypeProvisioning {
    fn default() -> Self {
        Self::oagw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_documented_type_catalogue() {
        let ids = served_gts_type_ids();

        assert_eq!(ids.len(), 6);
        assert!(ids.contains(&"gts.cf.core.oagw.upstream.v1~"));
        assert!(ids.contains(&"gts.cf.core.oagw.route.v1~"));
        assert!(ids.contains(&"gts.cf.core.oagw.auth_plugin.v1~"));
        assert!(ids.contains(&"gts.cf.core.oagw.guard_plugin.v1~"));
        assert!(ids.contains(&"gts.cf.core.oagw.transform_plugin.v1~"));
        assert!(ids.contains(&"gts.cf.core.errors.err.v1~"));
    }

    #[test]
    fn every_declared_id_is_a_unique_gts_base_type() {
        let ids = served_gts_type_ids();

        for id in ids {
            assert!(id.starts_with("gts."), "not a GTS id: {id}");
            assert!(id.ends_with('~'), "not a base type id: {id}");
        }
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "duplicate type ids");
    }

    #[test]
    fn provisioning_declaration_is_a_pure_value() {
        let provisioning = TypeProvisioning::oagw();

        assert_eq!(provisioning.served_type_ids().len(), 6);
        assert_eq!(provisioning.len(), 6);
        assert!(!provisioning.is_empty());
        assert_eq!(provisioning, TypeProvisioning::default());
    }
}
