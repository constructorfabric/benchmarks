//! GTS type provisioning (`DESIGN.md` § 3.1, `ADR/0008` § 4, `ADR/0009` § 4).
//!
//! Every type schema the gear owns is declared with `#[gts_type_schema]` in
//! [`gts_entries`], so the link-time `toolkit-gts` inventory carries them and
//! the types-registry seeds them at startup without any client round-trip.

/// Returns the GTS type schemas the gear contributes to the platform catalog.
///
/// The gear's own identifiers are declared through the `inventory` macros, so
/// this function only reports what the gear contributes — it exists so the
/// gear can log the count at startup and so tests can assert the set.
#[must_use]
pub fn declared_type_schemas() -> Vec<&'static str> {
    crate::domain::gts_helpers::TYPE_SCHEMA_IDS.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_and_plugin_type_is_declared() {
        let declared = declared_type_schemas();
        for expected in [
            crate::domain::gts_helpers::UPSTREAM_TYPE_ID,
            crate::domain::gts_helpers::ROUTE_TYPE_ID,
            crate::domain::gts_helpers::AUTH_PLUGIN_TYPE_ID,
            crate::domain::gts_helpers::GUARD_PLUGIN_TYPE_ID,
            crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE_ID,
            crate::domain::gts_helpers::PROTOCOL_TYPE_ID,
            crate::domain::gts_helpers::PROXY_TYPE_ID,
        ] {
            assert!(declared.contains(&expected), "{expected} is not declared");
        }
        assert_eq!(declared.len(), 7);
    }
}
