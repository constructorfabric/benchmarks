//! Plugin identifier parsing, classification, and resolution.
//!
//! Implements `cpt-cf-oagw-algo-plugin-identifier-parse` and
//! `cpt-cf-oagw-algo-plugin-resolve-ref` from
//! `docs/features/plugin-management.md` -- the shared identification model
//! `cpt-cf-oagw-feature-upstream-management` (2.2),
//! `cpt-cf-oagw-feature-route-management` (2.3), and
//! `cpt-cf-oagw-feature-plugin-execution` (2.9) reuse to accept a
//! `plugin_ref` value (in `plugins.items[]` / `auth.type`) without
//! executing it, and that this feature's own `GET`/`DELETE
//! /oagw/v1/plugins/{id}[/source]` reuse for the `{id}` path parameter.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use super::{Plugin, PluginType};

const GTS_PLUGIN_PREFIX: &str = "gts.cf.core.oagw.";
const GTS_VERSION_SUFFIX: &str = ".v1";
const NAMED_TOKEN_PREFIX: &str = "cf.core.oagw.";
const NAMED_TOKEN_SUFFIX: &str = ".v1";

/// Classification result of `cpt-cf-oagw-algo-plugin-identifier-parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginIdentifier {
    /// A UUID-backed (custom, stored) plugin identifier.
    ///
    /// `plugin_type` is `Some` when the candidate carried its own type
    /// context (a full GTS identifier, or an external type hint supplied by
    /// the caller); it is `None` only for a bare UUID `{id}` path parameter
    /// with no type segment, in which case the caller resolves `plugin_type`
    /// from the matching stored row instead (`inst-parse-bare-uuid-classify`).
    Uuid {
        plugin_type: Option<PluginType>,
        uuid: Uuid,
    },
    /// A named (registry-resolved, unstored) plugin identifier.
    Named {
        plugin_type: PluginType,
        token: String,
    },
}

/// Parse-failure classification for
/// `cpt-cf-oagw-algo-plugin-identifier-parse`.
///
/// Both variants are "parse failure" outcomes of the algorithm; this
/// feature's own management-API flows collapse both into the same `400`
/// response (`inst-plugin-get-if-malformed` treats any parse failure as "a
/// malformed identifier"), but they are kept distinct here so a
/// resolution-time caller can tell a syntactically malformed candidate apart
/// from a well-formed one carrying an unrecognized token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginIdentifierError {
    /// The candidate matches neither the bare-UUID form nor the
    /// `gts.cf.core.oagw.{type}_plugin.v1~{instance}` shape.
    Malformed,
    /// The candidate has the well-formed GTS plugin shape, but its instance
    /// part is neither a valid UUID nor a recognized named-plugin catalog
    /// token for the classified type.
    Unrecognized,
}

/// Strip the `_plugin` suffix from a `{type}_plugin` GTS type segment and
/// classify the remainder as a [`PluginType`].
fn parse_gts_type_segment(segment: &str) -> Option<PluginType> {
    PluginType::from_url_segment(segment.strip_suffix("_plugin")?)
}

/// Parse and classify a `plugin_ref` candidate string
/// (`cpt-cf-oagw-algo-plugin-identifier-parse`).
///
/// `type_hint` supplies `plugin_type` context for a bare-UUID candidate that
/// carries none of its own (the URL's `{type}_plugin` segment when present,
/// or a binding field's own type such as `auth.type`); pass `None` for a
/// bare UUID `{id}` path parameter with no type segment (`GET`/`DELETE
/// /oagw/v1/plugins/{id}[/source]`), which defers `plugin_type` resolution
/// to the matching stored row. A full GTS identifier always carries its own
/// type segment and ignores `type_hint`.
///
/// # Errors
///
/// Returns [`PluginIdentifierError::Malformed`] when `candidate` matches
/// neither the bare-UUID form nor the `gts.cf.core.oagw.{type}_plugin.v1~{instance}`
/// shape, and [`PluginIdentifierError::Unrecognized`] when it has that shape
/// but its instance part is neither a valid UUID nor a recognized
/// named-plugin catalog token for the classified type.
// @cpt-algo:cpt-cf-oagw-algo-plugin-identifier-parse:p2
// @cpt-dod:cpt-cf-oagw-dod-plugin-identification:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-return
#[allow(clippy::missing_panics_doc)]
pub fn parse_plugin_identifier(
    candidate: &str,
    type_hint: Option<PluginType>,
) -> Result<PluginIdentifier, PluginIdentifierError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-bare-uuid
    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-bare-uuid-classify
    if let Ok(uuid) = Uuid::parse_str(candidate) {
        return Ok(PluginIdentifier::Uuid {
            plugin_type: type_hint,
            uuid,
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-bare-uuid-classify
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-bare-uuid

    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-full-gts
    let Some(rest) = candidate.strip_prefix(GTS_PLUGIN_PREFIX) else {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-else-malformed
        // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-fail-malformed
        return Err(PluginIdentifierError::Malformed);
        // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-fail-malformed
        // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-else-malformed
    };

    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-split-instance
    let Some((type_segment, instance)) = rest.split_once('~') else {
        return Err(PluginIdentifierError::Malformed);
    };
    let Some(type_segment) = type_segment.strip_suffix(GTS_VERSION_SUFFIX) else {
        return Err(PluginIdentifierError::Malformed);
    };
    let Some(plugin_type) = parse_gts_type_segment(type_segment) else {
        return Err(PluginIdentifierError::Malformed);
    };
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-split-instance

    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-instance-uuid
    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-classify-uuid
    if let Ok(uuid) = Uuid::parse_str(instance) {
        return Ok(PluginIdentifier::Uuid {
            plugin_type: Some(plugin_type),
            uuid,
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-classify-uuid
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-instance-uuid

    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-named-token
    let token = instance
        .strip_prefix(NAMED_TOKEN_PREFIX)
        .and_then(|rest| rest.strip_suffix(NAMED_TOKEN_SUFFIX));
    let recognized = token.is_some_and(|token| plugin_type.named_catalog_entry(token).is_some());
    if recognized {
        // `token`/`recognized` guarantee this `unwrap` cannot fail.
        #[allow(clippy::unwrap_used)]
        let token = token.unwrap_or_default().to_owned();
        // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-classify-named
        return Ok(PluginIdentifier::Named { plugin_type, token });
        // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-classify-named
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-named-token

    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-else-unrecognized
    // @cpt-begin:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-fail-unrecognized
    Err(PluginIdentifierError::Unrecognized)
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-fail-unrecognized
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-else-unrecognized
    // @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-if-full-gts
}
// @cpt-end:cpt-cf-oagw-algo-plugin-identifier-parse:p2:inst-parse-return
// (the function's every path above returns the
// classification result or a parse failure -- there is no further step.)

/// Compose the anonymous GTS identifier for a UUID-backed custom plugin:
/// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` (`cpt-cf-oagw-dod-plugin-gts-issuance`).
#[must_use]
pub fn plugin_gts_ref(plugin_type: PluginType, uuid: Uuid) -> String {
    format!(
        "{GTS_PLUGIN_PREFIX}{}_plugin{GTS_VERSION_SUFFIX}~{uuid}",
        plugin_type.url_segment()
    )
}

/// Compose the GTS identifier for a named (registry-resolved) plugin:
/// `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{token}.v1`.
#[must_use]
pub fn named_plugin_gts_ref(plugin_type: PluginType, token: &str) -> String {
    format!(
        "{GTS_PLUGIN_PREFIX}{}_plugin{GTS_VERSION_SUFFIX}~{NAMED_TOKEN_PREFIX}{token}{NAMED_TOKEN_SUFFIX}",
        plugin_type.url_segment()
    )
}

/// Resolved binding descriptor, the documented output shape of
/// `cpt-cf-oagw-algo-plugin-resolve-ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPluginRef {
    /// The full anonymous GTS identifier for this binding.
    pub plugin_ref: String,
    /// `Some(uuid)` for a UUID-backed custom plugin; `None` for a named one.
    pub plugin_uuid: Option<Uuid>,
    /// `true` when this binding is backed by a stored `oagw_plugin` row.
    pub storage_backed: bool,
    /// The resolved plugin type (from the stored row for a UUID-backed
    /// binding, or from classification for a named one).
    pub plugin_type: PluginType,
}

/// Resolution-failure classification for
/// `cpt-cf-oagw-algo-plugin-resolve-ref`. The algorithm does not distinguish
/// a malformed candidate from an unresolvable one in its documented output
/// (both are "resolution failure"); the two are kept as separate variants
/// here purely so callers can render a `400` (malformed) versus a `404`
/// (unresolvable) without re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginResolutionError {
    /// The candidate failed `cpt-cf-oagw-algo-plugin-identifier-parse`.
    Malformed,
    /// The candidate parsed as `kind: "uuid"`, but no matching tenant-scoped
    /// `oagw_plugin` row exists, or its stored `plugin_type` disagrees with
    /// the classified one.
    Unresolvable,
}

/// Resolve a `plugin_ref` candidate against the tenant-scoped plugin store
/// (`cpt-cf-oagw-algo-plugin-resolve-ref`).
///
/// Invoked both by this feature's ID-addressed operations (`GET`/`DELETE
/// /oagw/v1/plugins/{id}[/source]`, via [`lookup_plugin_for_management`]
/// reusing the same [`parse_plugin_identifier`] step) and, at write time, by
/// upstream/route CRUD validating `plugins.items[]`/`auth.type` values.
///
/// # Errors
///
/// See [`PluginResolutionError`].
// @cpt-algo:cpt-cf-oagw-algo-plugin-resolve-ref:p2
pub fn resolve_plugin_ref(
    candidate: &str,
    type_hint: Option<PluginType>,
    tenant_id: Uuid,
    plugins: &DashMap<Uuid, Arc<Plugin>>,
) -> Result<ResolvedPluginRef, PluginResolutionError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-parse
    // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-if-parse-fail
    // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-fail-parse
    let identifier = parse_plugin_identifier(candidate, type_hint)
        .map_err(|_parse_error| PluginResolutionError::Malformed)?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-fail-parse
    // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-if-parse-fail
    // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-parse

    match identifier {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-if-uuid
        // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-uuid-query
        PluginIdentifier::Uuid { plugin_type, uuid } => {
            let stored = plugins
                .get(&uuid)
                .filter(|entry| entry.tenant_id == Some(tenant_id))
                .filter(|entry| plugin_type.is_none_or(|expected| expected == entry.plugin_type));
            // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-uuid-query
            let Some(stored) = stored else {
                // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-if-uuid-notfound
                // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-fail-uuid-notfound
                return Err(PluginResolutionError::Unresolvable);
                // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-fail-uuid-notfound
                // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-if-uuid-notfound
            };
            // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-else-uuid-found
            // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-return-uuid
            Ok(ResolvedPluginRef {
                plugin_ref: plugin_gts_ref(stored.plugin_type, uuid),
                plugin_uuid: Some(uuid),
                storage_backed: true,
                plugin_type: stored.plugin_type,
            })
            // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-return-uuid
            // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-else-uuid-found
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-if-uuid
        // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-else-named
        // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-named-no-lookup
        // @cpt-begin:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-return-named
        PluginIdentifier::Named { plugin_type, token } => Ok(ResolvedPluginRef {
            plugin_ref: named_plugin_gts_ref(plugin_type, &token),
            plugin_uuid: None,
            storage_backed: false,
            plugin_type,
        }),
        // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-return-named
        // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-named-no-lookup
        // @cpt-end:cpt-cf-oagw-algo-plugin-resolve-ref:p2:inst-resolve-else-named
    }
}

/// Lookup-failure classification shared by this feature's `GET`/`GET
/// .../source`/`DELETE /oagw/v1/plugins/{id}` flows
/// (`inst-plugin-get-parse` through `inst-plugin-get-if-notfound`, reused
/// verbatim by `cpt-cf-oagw-flow-plugin-get-source` and
/// `cpt-cf-oagw-flow-plugin-delete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginLookupError {
    /// `{id}` is malformed or carries an unrecognized token -- `400 ValidationError`.
    Malformed,
    /// `{id}` classifies as named (no stored row by design), or as a
    /// UUID-backed identifier with no matching tenant-scoped row, or (when
    /// `{id}` carried a `{type}_plugin` segment) a segment that disagrees
    /// with the stored row's `plugin_type` -- `404`, status and RFC 9457
    /// envelope only, no GTS `type` asserted.
    NotFound,
}

/// Shared `{id}`-path-parameter lookup for this feature's `GET`/`GET
/// .../source`/`DELETE /oagw/v1/plugins/{id}` flows.
///
/// `{id}` is accepted in either the bare-UUID form or the anonymous GTS form
/// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`; both resolve to the same
/// stored row's bare UUID `id`.
///
/// # Errors
///
/// See [`PluginLookupError`].
// @cpt-dod:cpt-cf-oagw-dod-plugin-tenant-scope:p1
pub fn lookup_plugin_for_management(
    id_param: &str,
    tenant_id: Uuid,
    plugins: &DashMap<Uuid, Arc<Plugin>>,
) -> Result<Arc<Plugin>, PluginLookupError> {
    let identifier = parse_plugin_identifier(id_param, None)
        .map_err(|_parse_error| PluginLookupError::Malformed)?;

    match identifier {
        PluginIdentifier::Named { .. } => Err(PluginLookupError::NotFound),
        PluginIdentifier::Uuid { plugin_type, uuid } => plugins
            .get(&uuid)
            .filter(|entry| entry.tenant_id == Some(tenant_id))
            .filter(|entry| plugin_type.is_none_or(|expected| expected == entry.plugin_type))
            .map(|entry| Arc::clone(entry.value()))
            .ok_or(PluginLookupError::NotFound),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample_plugin(id: Uuid, tenant_id: Uuid, plugin_type: PluginType, name: &str) -> Plugin {
        Plugin {
            id: Some(id),
            tenant_id: Some(tenant_id),
            plugin_type,
            name: name.to_owned(),
            description: None,
            config_schema: None,
            source_code: "def guard(req): return req".to_owned(),
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    #[test]
    fn bare_uuid_with_no_hint_classifies_as_uuid_with_no_type() {
        let uuid = Uuid::new_v4();
        let parsed = parse_plugin_identifier(&uuid.to_string(), None).unwrap();
        assert_eq!(
            parsed,
            PluginIdentifier::Uuid {
                plugin_type: None,
                uuid,
            }
        );
    }

    #[test]
    fn bare_uuid_with_a_type_hint_carries_it_through() {
        let uuid = Uuid::new_v4();
        let parsed = parse_plugin_identifier(&uuid.to_string(), Some(PluginType::Auth)).unwrap();
        assert_eq!(
            parsed,
            PluginIdentifier::Uuid {
                plugin_type: Some(PluginType::Auth),
                uuid,
            }
        );
    }

    #[test]
    fn full_gts_uuid_form_classifies_as_uuid_with_its_own_type() {
        let uuid = Uuid::new_v4();
        let candidate = plugin_gts_ref(PluginType::Guard, uuid);
        let parsed = parse_plugin_identifier(&candidate, None).unwrap();
        assert_eq!(
            parsed,
            PluginIdentifier::Uuid {
                plugin_type: Some(PluginType::Guard),
                uuid,
            }
        );
    }

    #[test]
    fn full_gts_named_form_classifies_as_named() {
        let candidate = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
        let parsed = parse_plugin_identifier(candidate, None).unwrap();
        assert_eq!(
            parsed,
            PluginIdentifier::Named {
                plugin_type: PluginType::Auth,
                token: "apikey".to_owned(),
            }
        );
    }

    #[test]
    fn all_twelve_named_catalog_tokens_parse_as_named() {
        for plugin_type in [PluginType::Auth, PluginType::Guard, PluginType::Transform] {
            for entry in plugin_type.named_catalog() {
                let candidate = named_plugin_gts_ref(plugin_type, entry.token);
                let parsed = parse_plugin_identifier(&candidate, None).unwrap();
                assert_eq!(
                    parsed,
                    PluginIdentifier::Named {
                        plugin_type,
                        token: entry.token.to_owned(),
                    }
                );
            }
        }
    }

    #[test]
    fn syntactically_invalid_identifier_is_malformed() {
        let err = parse_plugin_identifier("not-a-plugin-ref", None).unwrap_err();
        assert_eq!(err, PluginIdentifierError::Malformed);
    }

    #[test]
    fn wrong_gts_root_is_malformed() {
        let err = parse_plugin_identifier("gts.cf.core.oagw.upstream.v1~123", None).unwrap_err();
        assert_eq!(err, PluginIdentifierError::Malformed);
    }

    #[test]
    fn well_formed_gts_with_unrecognized_token_is_unrecognized() {
        let err = parse_plugin_identifier(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.frobnicate.v1",
            None,
        )
        .unwrap_err();
        assert_eq!(err, PluginIdentifierError::Unrecognized);
    }

    #[test]
    fn resolve_ref_succeeds_for_every_named_catalog_token_with_no_stored_row() {
        let tenant_id = Uuid::new_v4();
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        for plugin_type in [PluginType::Auth, PluginType::Guard, PluginType::Transform] {
            for entry in plugin_type.named_catalog() {
                let candidate = named_plugin_gts_ref(plugin_type, entry.token);
                let resolved = resolve_plugin_ref(&candidate, None, tenant_id, &plugins).unwrap();
                assert!(!resolved.storage_backed);
                assert_eq!(resolved.plugin_uuid, None);
                assert_eq!(resolved.plugin_type, plugin_type);
            }
        }
    }

    #[test]
    fn a_catalog_only_token_still_resolves_as_named_but_is_not_backed() {
        // `cpt-cf-oagw-algo-plugin-resolve-ref` is identification-only: it
        // resolves every recognized named token (acceptance criterion:
        // "all twelve ... resolve successfully"). Distinguishing which of
        // those are *executable* is `NamedPluginCatalogEntry::has_backing_implementation`,
        // not a resolution failure -- so a catalog-only token like `basic`
        // resolves via `resolve_plugin_ref` but does not resolve to a
        // backing implementation via the catalog entry.
        let tenant_id = Uuid::new_v4();
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        let candidate = named_plugin_gts_ref(PluginType::Auth, "basic");

        let resolved = resolve_plugin_ref(&candidate, None, tenant_id, &plugins).unwrap();
        assert!(!resolved.storage_backed);

        let catalog_entry = PluginType::Auth.named_catalog_entry("basic").unwrap();
        assert!(!catalog_entry.has_backing_implementation);
    }

    #[test]
    fn resolve_ref_finds_a_tenant_owned_stored_uuid_plugin() {
        let tenant_id = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin = sample_plugin(uuid, tenant_id, PluginType::Guard, "my-guard");
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        plugins.insert(uuid, Arc::new(plugin));

        let resolved = resolve_plugin_ref(&uuid.to_string(), None, tenant_id, &plugins).unwrap();
        assert!(resolved.storage_backed);
        assert_eq!(resolved.plugin_uuid, Some(uuid));
        assert_eq!(resolved.plugin_type, PluginType::Guard);
    }

    #[test]
    fn resolve_ref_rejects_a_stored_uuid_plugin_owned_by_another_tenant() {
        let owner_tenant = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin = sample_plugin(uuid, owner_tenant, PluginType::Guard, "my-guard");
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        plugins.insert(uuid, Arc::new(plugin));

        let err = resolve_plugin_ref(&uuid.to_string(), None, other_tenant, &plugins).unwrap_err();
        assert_eq!(err, PluginResolutionError::Unresolvable);
    }

    #[test]
    fn resolve_ref_rejects_a_type_segment_mismatch() {
        let tenant_id = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin = sample_plugin(uuid, tenant_id, PluginType::Guard, "my-guard");
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        plugins.insert(uuid, Arc::new(plugin));

        let candidate = plugin_gts_ref(PluginType::Auth, uuid);
        let err = resolve_plugin_ref(&candidate, None, tenant_id, &plugins).unwrap_err();
        assert_eq!(err, PluginResolutionError::Unresolvable);
    }

    #[test]
    fn resolve_ref_propagates_malformed_parse_failures() {
        let tenant_id = Uuid::new_v4();
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        let err = resolve_plugin_ref("not-a-plugin-ref", None, tenant_id, &plugins).unwrap_err();
        assert_eq!(err, PluginResolutionError::Malformed);
    }

    #[test]
    fn lookup_for_management_accepts_both_id_forms_for_the_same_plugin() {
        let tenant_id = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin = sample_plugin(uuid, tenant_id, PluginType::Transform, "req-id");
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        plugins.insert(uuid, Arc::new(plugin));

        let by_bare_uuid =
            lookup_plugin_for_management(&uuid.to_string(), tenant_id, &plugins).unwrap();
        let gts_form = plugin_gts_ref(PluginType::Transform, uuid);
        let by_gts_form = lookup_plugin_for_management(&gts_form, tenant_id, &plugins).unwrap();

        assert_eq!(by_bare_uuid.id, Some(uuid));
        assert_eq!(by_gts_form.id, Some(uuid));
    }

    #[test]
    fn lookup_for_management_returns_not_found_for_a_named_identifier() {
        let tenant_id = Uuid::new_v4();
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        let candidate = named_plugin_gts_ref(PluginType::Auth, "apikey");
        let err = lookup_plugin_for_management(&candidate, tenant_id, &plugins).unwrap_err();
        assert_eq!(err, PluginLookupError::NotFound);
    }

    #[test]
    fn lookup_for_management_returns_bad_request_for_a_malformed_identifier() {
        let tenant_id = Uuid::new_v4();
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        let err =
            lookup_plugin_for_management("not-a-plugin-ref", tenant_id, &plugins).unwrap_err();
        assert_eq!(err, PluginLookupError::Malformed);
    }

    #[test]
    fn lookup_for_management_returns_not_found_for_another_tenants_plugin() {
        let owner_tenant = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin = sample_plugin(uuid, owner_tenant, PluginType::Guard, "my-guard");
        let plugins: DashMap<Uuid, Arc<Plugin>> = DashMap::new();
        plugins.insert(uuid, Arc::new(plugin));

        let err =
            lookup_plugin_for_management(&uuid.to_string(), other_tenant, &plugins).unwrap_err();
        assert_eq!(err, PluginLookupError::NotFound);
    }
}
