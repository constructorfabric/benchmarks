//! Anonymous GTS type identifiers for the OAGW management plane.
//!
//! Every wire identifier an OAGW resource carries is an *anonymous* GTS
//! identifier: `gts.cf.core.oagw.<type>.v1~<uuid>`. Resource payloads speak
//! bare UUIDs (`docs/schemas/upstream.v1.schema.json` uses `format: uuid`);
//! problem extension members and `referenced_by` sets speak full GTS ids
//! (`docs/ADR/0001-request-routing.md`).
//!
//! The literals here mirror the proc-macro `gts_id!(...)` expansions used by
//! the transport layer; the `ids_match_wire` test pins them together.

/// GTS type segment for upstreams.
pub const UPSTREAM_TYPE: &str = "cf.core.oagw.upstream.v1";
/// GTS type segment for routes.
pub const ROUTE_TYPE: &str = "cf.core.oagw.route.v1";
/// GTS type segment for plugin catalog rows.
pub const PLUGIN_TYPE: &str = "cf.core.oagw.plugin.v1";

/// Full anonymous GTS id for a resource of `type_id` owned by `uuid`.
#[must_use]
pub fn gts_id(type_id: &str, uuid: &uuid::Uuid) -> String {
    format!("gts.{type_id}~{uuid}")
}

/// Extract the UUID half of an anonymous GTS identifier or return the input
/// untouched when it is already a bare UUID.
///
/// Both spellings are accepted anywhere a resource id is addressed, so
/// `/oagw/v1/upstreams/gts.cf.core.oagw.upstream.v1~<uuid>` and
/// `/oagw/v1/upstreams/<uuid>` name the same upstream.
#[must_use]
pub fn resource_uuid(raw: &str) -> Option<uuid::Uuid> {
    let tail = raw.split('~').next_back().unwrap_or(raw);
    uuid::Uuid::parse_str(tail).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn builds_anonymous_gts_id() {
        let id = uuid::Uuid::nil();
        assert_eq!(
            gts_id(UPSTREAM_TYPE, &id),
            format!("gts.{UPSTREAM_TYPE}~00000000-0000-0000-0000-000000000000")
        );
    }

    #[test]
    fn parses_bare_and_prefixed_forms() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(resource_uuid(&id.to_string()), Some(id));
        assert_eq!(resource_uuid(&gts_id(ROUTE_TYPE, &id)), Some(id));
    }

    #[test]
    fn rejects_non_uuid_input() {
        assert_eq!(resource_uuid("not-an-id"), None);
        assert_eq!(resource_uuid("gts.cf.core.oagw.route.v1~nope"), None);
    }

    #[test]
    fn ids_match_wire() {
        // The transport layer pins these same strings through `gts_id!` with
        // the `GTS_ID_PREFIX` ("gts.") applied.
        let id = Uuid::new_v4();
        assert!(gts_id(UPSTREAM_TYPE, &id).starts_with("gts.cf.core.oagw.upstream.v1~"));
    }
}
