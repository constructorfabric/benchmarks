//! The plugin catalog, binding validation and the served guard plugins.
//!
//! Realizes `cpt-cf-oagw-algo-pm-validate-binding`,
//! `cpt-cf-oagw-dod-pm-catalog`, `cpt-cf-oagw-dod-pm-binding-validation` and
//! `cpt-cf-oagw-algo-tp-required-headers-guard`.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::PluginKind;

/// A built-in plugin catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The GTS identifier callers bind.
    pub id: &'static str,
    /// Which phase it participates in.
    pub kind: PluginKind,
    /// Whether an implementation actually backs it in this configuration.
    pub served: bool,
}

/// The built-in plugin catalog.
///
/// Entries marked `served: false` are catalog-only: the identifier is known but
/// no implementation backs it, and binding one is rejected at write time.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1",
        kind: PluginKind::Auth,
        served: true,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        kind: PluginKind::Auth,
        served: true,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
        kind: PluginKind::Auth,
        served: true,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
        kind: PluginKind::Auth,
        served: true,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        kind: PluginKind::Auth,
        served: false,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        kind: PluginKind::Auth,
        served: false,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        kind: PluginKind::Guard,
        served: true,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        kind: PluginKind::Guard,
        served: false,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        kind: PluginKind::Guard,
        served: false,
    },
    CatalogEntry {
        id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
        kind: PluginKind::Transform,
        served: true,
    },
];

/// Look a catalog entry up by identifier.
#[must_use]
pub fn catalog_entry(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// The instance part of a plugin binding entry.
///
/// The frozen upstream schema admits either a full GTS identifier or a bare
/// UUID; both resolve to the same custom plugin definition. The route schema
/// admits only the GTS form.
#[must_use]
pub fn instance_uuid(entry: &str) -> Option<Uuid> {
    let candidate = entry.rsplit_once('~').map_or(entry, |(_, tail)| tail);
    Uuid::parse_str(candidate).ok()
}

/// The outcome of resolving one binding entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// A served built-in.
    Builtin(&'static CatalogEntry),
    /// A tenant-owned custom definition.
    Custom(Uuid),
}

/// Resolve one plugin binding entry.
///
/// # Errors
/// Returns [`DomainError::Validation`] naming the array index when the entry
/// does not resolve, names a catalog-only identifier, or names an auth plugin
/// in the ordered guard/transform list.
// @cpt-begin:cpt-cf-oagw-dod-pm-binding-validation:p1:inst-full
pub fn resolve_binding(
    field: &str,
    index: usize,
    entry: &str,
    custom_exists: impl Fn(Uuid) -> Option<PluginKind>,
) -> Result<Binding, DomainError> {
    let at = format!("{field}[{index}]");

    if let Some(e) = catalog_entry(entry) {
        if !e.served {
            return Err(DomainError::validation(
                at,
                format!("`{entry}` is a catalog-only identifier with no backing implementation"),
            ));
        }
        if e.kind == PluginKind::Auth {
            return Err(DomainError::validation(
                at,
                "an auth plugin is bound through the upstream's `auth` field, \
                 not the ordered plugin list",
            ));
        }
        return Ok(Binding::Builtin(e));
    }

    // Either a full GTS identifier wrapping a UUID, or a bare UUID.
    if let Some(id) = instance_uuid(entry) {
        return match custom_exists(id) {
            Some(PluginKind::Auth) => Err(DomainError::validation(
                at,
                "an auth plugin is bound through the upstream's `auth` field, \
                 not the ordered plugin list",
            )),
            Some(_) => Ok(Binding::Custom(id)),
            None => Err(DomainError::validation(
                at,
                format!("`{entry}` does not resolve to a known plugin definition"),
            )),
        };
    }

    Err(DomainError::validation(
        at,
        format!("`{entry}` does not resolve to a known plugin identifier"),
    ))
}
// @cpt-end:cpt-cf-oagw-dod-pm-binding-validation:p1:inst-full

/// Resolve an upstream's auth plugin identifier.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the identifier is unknown, is
/// catalog-only, or is not an auth plugin.
pub fn resolve_auth_plugin(
    entry: &str,
    custom_exists: impl Fn(Uuid) -> Option<PluginKind>,
) -> Result<Binding, DomainError> {
    if let Some(e) = catalog_entry(entry) {
        if !e.served {
            return Err(DomainError::validation(
                "auth.type",
                format!("`{entry}` is a catalog-only identifier with no backing implementation"),
            ));
        }
        if e.kind != PluginKind::Auth {
            return Err(DomainError::validation(
                "auth.type",
                format!("`{entry}` is not an auth plugin"),
            ));
        }
        return Ok(Binding::Builtin(e));
    }
    if let Some(id) = instance_uuid(entry) {
        return match custom_exists(id) {
            Some(PluginKind::Auth) => Ok(Binding::Custom(id)),
            Some(_) => Err(DomainError::validation(
                "auth.type",
                format!("`{entry}` is not an auth plugin"),
            )),
            None => Err(DomainError::validation(
                "auth.type",
                format!("`{entry}` does not resolve to a known plugin definition"),
            )),
        };
    }
    Err(DomainError::validation(
        "auth.type",
        format!("`{entry}` does not resolve to a known plugin identifier"),
    ))
}

/// Parse a comma-separated required-header list.
///
/// Entries are trimmed and lowercased and empty entries dropped; an absent or
/// all-blank list makes the phase a no-op.
#[must_use]
pub fn parse_required_headers(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// The first required header missing from `present`, if any.
///
/// Only the first missing header is reported, per ADR-0009.
// @cpt-begin:cpt-cf-oagw-dod-tp-required-headers-guard:p1:inst-full
#[must_use]
pub fn first_missing_header<'a>(required: &'a [String], present: &[String]) -> Option<&'a String> {
    required
        .iter()
        .find(|r| !present.iter().any(|p| p.eq_ignore_ascii_case(r)))
}
// @cpt-end:cpt-cf-oagw-dod-tp-required-headers-guard:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;

    fn no_custom(_: Uuid) -> Option<PluginKind> {
        None
    }

    #[test]
    fn a_served_guard_builtin_binds() {
        let b = resolve_binding(
            "plugins.items",
            0,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            no_custom,
        )
        .unwrap();
        assert!(matches!(b, Binding::Builtin(e) if e.served));
    }

    #[test]
    fn a_catalog_only_identifier_is_rejected() {
        let err = resolve_binding(
            "plugins.items",
            2,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
            no_custom,
        )
        .unwrap_err();
        match err {
            DomainError::Validation { field, message } => {
                assert_eq!(field, "plugins.items[2]");
                assert!(message.contains("catalog-only"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unresolvable_identifier_is_rejected_naming_its_index() {
        let err = resolve_binding("plugins.items", 1, "not-a-plugin", no_custom).unwrap_err();
        assert!(matches!(err, DomainError::Validation { ref field, .. } if field == "plugins.items[1]"));
    }

    #[test]
    fn both_the_gts_and_bare_uuid_forms_resolve_to_the_same_definition() {
        let id = Uuid::new_v4();
        let exists = move |q: Uuid| (q == id).then_some(PluginKind::Guard);

        let bare = resolve_binding("plugins.items", 0, &id.to_string(), exists).unwrap();
        let gts = resolve_binding(
            "plugins.items",
            0,
            &format!("gts.cf.core.oagw.guard_plugin.v1~{id}"),
            exists,
        )
        .unwrap();
        assert_eq!(bare, gts);
        assert_eq!(bare, Binding::Custom(id));
    }

    #[test]
    fn an_unknown_custom_uuid_is_rejected() {
        assert!(resolve_binding("plugins.items", 0, &Uuid::new_v4().to_string(), no_custom).is_err());
    }

    #[test]
    fn an_auth_plugin_cannot_sit_in_the_ordered_list() {
        let err = resolve_binding(
            "plugins.items",
            0,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            no_custom,
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));
    }

    #[test]
    fn auth_field_accepts_a_served_auth_builtin() {
        resolve_auth_plugin(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            no_custom,
        )
        .unwrap();
    }

    #[test]
    fn auth_field_rejects_a_guard_identifier_and_catalog_only_auth() {
        assert!(
            resolve_auth_plugin(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                no_custom
            )
            .is_err()
        );
        assert!(
            resolve_auth_plugin(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
                no_custom
            )
            .is_err()
        );
    }

    #[test]
    fn required_header_lists_are_trimmed_lowercased_and_compacted() {
        assert_eq!(
            parse_required_headers(Some(" X-A , x-b ,, ")),
            vec!["x-a".to_owned(), "x-b".to_owned()]
        );
        assert!(parse_required_headers(Some(", , ,")).is_empty());
        assert!(parse_required_headers(None).is_empty());
    }

    #[test]
    fn only_the_first_missing_header_is_reported_in_declared_order() {
        let required = parse_required_headers(Some("x-a,x-b,x-c"));
        let present = vec!["X-A".to_owned()];
        assert_eq!(
            first_missing_header(&required, &present).map(String::as_str),
            Some("x-b")
        );
    }

    #[test]
    fn a_satisfied_list_reports_nothing_missing() {
        let required = parse_required_headers(Some("x-a"));
        let present = vec!["x-a".to_owned()];
        assert!(first_missing_header(&required, &present).is_none());
    }

    #[test]
    fn an_empty_required_list_is_a_no_op() {
        assert!(first_missing_header(&[], &[]).is_none());
    }
}
