//! Identifier well-formedness helpers: the `{id}` path-parameter dual-form
//! reconciliation (bare UUID vs. anonymous GTS identifier), the fixed
//! `protocol` enum, and GTS-identifier/UUID well-formedness checks used by
//! `auth.type` and `plugins.items[]` validation.

use uuid::Uuid;

/// Prefix of the anonymous GTS form of an Upstream identifier, per
/// `docs/features/upstream-management.md`'s `{id}` dual-form reconciliation:
/// `GET`/`PUT`/`DELETE /oagw/v1/upstreams/{id}` accept either the bare UUID
/// or `gts.cf.core.oagw.upstream.v1~{uuid}`.
const UPSTREAM_GTS_ID_PREFIX: &str = "gts.cf.core.oagw.upstream.v1~";

/// Normalize a `{id}` path-parameter value -- bare UUID or the anonymous GTS
/// form -- to its bare UUID, or `None` when neither form parses.
#[must_use]
pub fn normalize_upstream_path_id(raw: &str) -> Option<Uuid> {
    let candidate = raw.strip_prefix(UPSTREAM_GTS_ID_PREFIX).unwrap_or(raw);
    Uuid::parse_str(candidate).ok()
}

/// The two `protocol` values `upstream.v1.schema.json` allows.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

#[must_use]
pub fn is_valid_protocol(value: &str) -> bool {
    value == PROTOCOL_HTTP || value == PROTOCOL_GRPC
}

/// Well-formedness (not resolvability) of a GTS identifier string, as
/// `auth.type` and `plugins.items[]` require. Resolving whether it names an
/// installed plugin/auth type is `cpt-cf-oagw-feature-plugin-execution`'s
/// (2.9) concern.
#[must_use]
pub fn is_well_formed_gts_identifier(value: &str) -> bool {
    gts::GtsInstanceId::try_new(value).is_ok()
}

/// `plugins.items[]` entries are either a well-formed GTS identifier
/// (builtin plugin) or a well-formed UUID (custom plugin).
#[must_use]
pub fn is_well_formed_plugin_ref(value: &str) -> bool {
    Uuid::parse_str(value).is_ok() || is_well_formed_gts_identifier(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_the_bare_uuid_form() {
        let id = Uuid::new_v4();
        assert_eq!(normalize_upstream_path_id(&id.to_string()), Some(id));
    }

    #[test]
    fn normalizes_the_anonymous_gts_form() {
        let id = Uuid::new_v4();
        let gts_form = format!("gts.cf.core.oagw.upstream.v1~{id}");
        assert_eq!(normalize_upstream_path_id(&gts_form), Some(id));
    }

    #[test]
    fn rejects_neither_form() {
        assert_eq!(normalize_upstream_path_id("not-an-id"), None);
    }

    #[test]
    fn validates_the_two_documented_protocol_values() {
        assert!(is_valid_protocol(PROTOCOL_HTTP));
        assert!(is_valid_protocol(PROTOCOL_GRPC));
        assert!(!is_valid_protocol(
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.quic.v1"
        ));
    }

    #[test]
    fn accepts_well_formed_gts_identifiers_for_auth_type_examples() {
        assert!(is_well_formed_gts_identifier(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        ));
    }

    #[test]
    fn rejects_a_malformed_gts_identifier() {
        assert!(!is_well_formed_gts_identifier("not a gts id"));
    }

    #[test]
    fn plugin_ref_accepts_uuid_or_gts_identifier_only() {
        assert!(is_well_formed_plugin_ref(&Uuid::new_v4().to_string()));
        assert!(is_well_formed_plugin_ref(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        ));
        assert!(!is_well_formed_plugin_ref("neither-uuid-nor-gts"));
    }
}
