//! The conflict-status resolution of the management surface
//! (`cpt-cf-oagw-algo-conflict-status`).
//!
//! The algorithm classifies the violation kind a domain or alias call raised
//! against the operation that raised it and decides the status and GTS `type`
//! of the rendered response. It adds no kind, no row and no GTS error type: a
//! conflict the surface elevates to 409 keeps the GTS type the existing
//! `ValidationError` row carries, and the plugin-delete conflict selects the
//! existing `PluginInUse` row.

// @cpt-begin:cpt-cf-oagw-dod-error-status-decision:p1:inst-full

use uuid::Uuid;

use crate::domain::error::{DomainError, OagwError, ViolationKind};
use crate::infra::storage::{PluginReference, ResourceKind};

/// The GTS type the `ValidationError` row carries, which the alias and
/// route-match conflicts keep when they are elevated to 409.
pub const CONFLICT_GTS_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";

/// The operation context a violation is resolved against: the branch the
/// algorithm selects depends on it, not on the kind alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// An upstream create or replace, carrying the tenant whose
    /// `(tenant_id, alias)` key the alias contract checked.
    UpstreamWrite { tenant_id: Uuid },
    /// A route create or replace, carrying the rendered `(path, priority,
    /// method)` match key of the candidate.
    RouteWrite { match_key: String },
    /// A plugin delete, carrying the identifier of the plugin and the scan of
    /// the bindings that still reference it.
    PluginDelete {
        plugin_id: String,
        referenced_by: Vec<PluginReference>,
    },
    /// Every other operation: the mapping table alone decides.
    Other,
}

/// Resolves the status and GTS `type` of the response one violation is
/// rendered with (`cpt-cf-oagw-algo-conflict-status`).
#[must_use]
pub fn resolve(error: &DomainError, operation: &Operation) -> OagwError {
    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-10
    // The status and GTS `type` this resolution decides are returned to the
    // calling flow; every step below computes them for one violation.
    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-01
    // The kind is classified against the closed vocabulary of the domain
    // error contract; the algorithm declares no kind of its own.
    let kind = error.kind();
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-01

    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-02
    // An `already-exists` raised by an upstream write is a per-tenant alias
    // conflict or a duplicate key that reached the store from a path the alias
    // check did not render; both are elevated to 409.
    if kind == Some(ViolationKind::AlreadyExists)
        && matches!(operation, Operation::UpstreamWrite { .. })
    {
        // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-03
        // 409, keeping the GTS type the domain already produces for the kind,
        // with the body's `status` field carrying 409 and the detail naming
        // the colliding `(tenant_id, alias)` key, which the alias contract
        // already renders in its message.
        return OagwError::validation_error(render(error)).with_status(409);
        // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-03
    }
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-02

    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-04
    // The route-match uniqueness invariant: a second route with the same
    // `path`, `priority` and `method` under the same upstream.
    if kind == Some(ViolationKind::AlreadyExists)
        && let Operation::RouteWrite { match_key } = operation
    {
        // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-05
        // 409 with the same GTS type and a detail naming the colliding
        // `(path, priority, method)` key.
        return OagwError::validation_error(format!(
            "an enabled route with this match key already exists under the same upstream: \
             {match_key}"
        ))
        .with_status(409);
        // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-05
    }
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-04

    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-06
    // A plugin delete whose reference scan found a binding.
    if let Operation::PluginDelete {
        plugin_id,
        referenced_by,
    } = operation
        && !referenced_by.is_empty()
    {
        // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-07
        // The existing `PluginInUse` row, with the `plugin_id` field and the
        // `referenced_by` object naming the referencing resources.
        return OagwError::plugin_in_use(format!(
            "the plugin '{plugin_id}' is still referenced by an upstream or a route of the \
             caller's tenant, so it is left stored"
        ))
        .with_plugin_id(plugin_id.clone())
        .with_referenced_by(render_references(referenced_by));
        // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-07
    }
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-06

    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-08
    // Every other violation is passed to the mapping table unchanged: a
    // not-found kind renders 404 and every other kind the 400 `ValidationError`
    // row the domain contract already assigns.
    // @cpt-begin:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-09
    // The rendered response the calling flow produces from this error carries
    // `X-OAGW-Error-Source: gateway`, attached at render time by
    // `cpt-cf-oagw-flow-error-response`, and no row is added to the mapping
    // table, which stays closed at 22 rows.
    OagwError::from(error)
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-09
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-08
    // @cpt-end:cpt-cf-oagw-algo-conflict-status:p1:inst-cs-10
}

/// The detail a rendered conflict carries: the domain message, which names the
/// colliding key the raising layer already rendered.
fn render(error: &DomainError) -> String {
    format!("oagw.domain: {}", error.message())
}

/// Renders the `referenced_by` extension member of a `PluginInUse` body.
#[must_use]
pub fn render_references(references: &[PluginReference]) -> serde_json::Value {
    let mut upstreams: Vec<String> = Vec::new();
    let mut routes: Vec<String> = Vec::new();
    for reference in references {
        let id = reference.id.to_string();
        match reference.kind {
            ResourceKind::Upstream => upstreams.push(id),
            ResourceKind::Route => routes.push(id),
            ResourceKind::Plugin => {}
        }
    }
    upstreams.sort();
    routes.sort();
    serde_json::json!({ "upstreams": upstreams, "routes": routes })
}

// @cpt-end:cpt-cf-oagw-dod-error-status-decision:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::{MAPPING_TABLE, Violation};

    /// A per-tenant alias conflict, raised by the alias contract for an alias
    /// another upstream of the same tenant already holds.
    fn alias_conflict() -> DomainError {
        use crate::domain::alias::normalize_and_enforce;
        use crate::domain::model::{Endpoint, PROTOCOL_HTTP, ServerConfig, Upstream};
        use crate::domain::repo::UpstreamRepository;

        let tenant_id = Uuid::from_u128(0x1);
        let stores = crate::infra::storage::InMemoryStores::new();
        stores
            .upstreams()
            .insert(&Upstream {
                id: Some(Uuid::from_u128(0x2)),
                tenant_id: Some(tenant_id),
                alias: Some("taken.vendor.com".to_owned()),
                protocol: Some(PROTOCOL_HTTP.to_owned()),
                server: Some(ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: "https".to_owned(),
                        host: Some("api.vendor.com".to_owned()),
                        port: 443,
                    }],
                }),
                ..Upstream::default()
            })
            .expect("the holder is valid");
        normalize_and_enforce("taken.vendor.com", tenant_id, None, &stores.upstreams())
            .expect_err("the alias is taken in the same tenant")
    }

    #[test]
    fn an_alias_conflict_is_elevated_to_409_keeping_the_validation_type() {
        let error = resolve(
            &alias_conflict(),
            &Operation::UpstreamWrite {
                tenant_id: Uuid::from_u128(0x1),
            },
        );
        assert_eq!(error.effective_status(), 409);
        assert_eq!(error.gts_type(), CONFLICT_GTS_TYPE);
        assert_eq!(error.mapping().status, 400, "the row itself stays 400");
        assert!(error.has_status_override());
        assert!(error.detail().contains("taken.vendor.com"));
        assert!(
            error
                .detail()
                .contains("00000000-0000-0000-0000-000000000001")
        );
    }

    #[test]
    fn a_route_match_conflict_is_elevated_to_409_naming_the_match_key() {
        let error = resolve(
            &DomainError::already_exists("match.http.path", "duplicate"),
            &Operation::RouteWrite {
                match_key: "path '/api/a', priority 10, method 'GET'".to_owned(),
            },
        );
        assert_eq!(error.effective_status(), 409);
        assert_eq!(error.gts_type(), CONFLICT_GTS_TYPE);
        assert!(
            error
                .detail()
                .contains("path '/api/a', priority 10, method 'GET'")
        );
    }

    #[test]
    fn a_referenced_plugin_delete_selects_the_existing_plugin_in_use_row() {
        let references = vec![PluginReference {
            kind: ResourceKind::Route,
            id: Uuid::from_u128(0x2),
            binding_name: "gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-0000000000aa"
                .to_owned(),
        }];
        let error = resolve(
            &DomainError::not_found("id", "unused"),
            &Operation::PluginDelete {
                plugin_id: "gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-0000000000aa"
                    .to_owned(),
                referenced_by: references.clone(),
            },
        );
        assert_eq!(error.effective_status(), 409);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
        );
        assert_eq!(error.mapping().status, 409);
        assert!(!error.has_status_override(), "the row already carries 409");
        assert_eq!(
            error.context().plugin_id.as_deref(),
            Some("gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-0000000000aa")
        );
        assert_eq!(
            error.context().referenced_by.as_ref(),
            Some(&render_references(&references))
        );
    }

    #[test]
    fn an_unreferenced_plugin_delete_is_not_a_conflict() {
        let error = resolve(
            &DomainError::not_found(
                "id",
                "no plugin with this identifier exists in the caller's tenant",
            ),
            &Operation::PluginDelete {
                plugin_id: "p".to_owned(),
                referenced_by: Vec::new(),
            },
        );
        assert_eq!(error.effective_status(), 404);
        assert!(error.context().plugin_id.is_none());
        assert!(error.context().referenced_by.is_none());
    }

    #[test]
    fn every_other_violation_is_passed_to_the_mapping_table_unchanged() {
        let not_found = DomainError::not_found("id", "absent");
        let error = resolve(&not_found, &Operation::Other);
        assert_eq!(error.effective_status(), 404);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );

        let unknown = DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "tags[0]",
            "not a string",
        ));
        let error = resolve(&unknown, &Operation::Other);
        assert_eq!(error.effective_status(), 400);
        assert_eq!(error.gts_type(), CONFLICT_GTS_TYPE);
    }

    #[test]
    fn the_resolution_adds_no_row_to_the_mapping_table() {
        assert_eq!(MAPPING_TABLE.len(), 22, "the mapping table stays closed");
    }

    #[test]
    fn the_rendered_references_are_sorted_and_grouped_by_kind() {
        let references = vec![
            PluginReference {
                kind: ResourceKind::Route,
                id: Uuid::from_u128(0x9),
                binding_name: String::new(),
            },
            PluginReference {
                kind: ResourceKind::Upstream,
                id: Uuid::from_u128(0x3),
                binding_name: String::new(),
            },
        ];
        let rendered = render_references(&references);
        assert_eq!(
            rendered,
            serde_json::json!({
                "upstreams": ["00000000-0000-0000-0000-000000000003"],
                "routes": ["00000000-0000-0000-0000-000000000009"],
            })
        );
    }
}
