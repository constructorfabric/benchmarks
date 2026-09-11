//! Chain assembly and ordering (`cpt-cf-oagw-algo-plugin-chain-assemble`).
//!
//! Concatenates the upstream-bound and route-bound guard/transform binding
//! lists -- upstream first, preserving each list's own `position` order,
//! never deduplicated or reordered by kind (`cpt-cf-oagw-dod-plugin-chain-concat`)
//! -- resolves every entry, and resolves the single optional auth binding
//! separately (there is at most one auth plugin per request, so no
//! ordering question arises for that kind).

#[cfg(test)]
use serde_json::Value;

use crate::model::plugin::PluginType;
use crate::model::plugin::identity::{PluginIdentifier, parse_plugin_identifier};

use super::binding::{
    BindingResolutionError, PluginBinding, ResolvedAuth, ResolvedGuard, ResolvedTransform,
    resolve_auth_binding, resolve_guard_binding, resolve_transform_binding,
};
use super::registry::Registries;

/// The assembled execution plan: at most one resolved auth plugin, one
/// ordered guard list, and one ordered transform list
/// (`cpt-cf-oagw-algo-plugin-chain-assemble`'s output shape).
#[derive(Debug)]
pub(crate) struct ExecutionPlan {
    pub auth: Option<ResolvedAuth>,
    pub guards: Vec<ResolvedGuard>,
    pub transforms: Vec<ResolvedTransform>,
}

/// Assemble the execution plan from the resolved bindings
/// (`inst-chain-assemble-01` through `-10`).
///
/// `upstream_bindings` and `route_bindings` are each already in their own
/// `position` order; this function concatenates upstream-then-route
/// (`[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`,
/// `cpt-cf-oagw-dod-plugin-chain-concat`) and resolves each entry in that
/// order, classifying it as a guard or a transform by its own embedded
/// GTS kind segment. The first resolution failure abandons assembly
/// (`inst-chain-assemble-04`/`-05`); a binding whose kind cannot be
/// classified as guard or transform at all (e.g. an `auth_plugin`
/// identifier bound at this position, or a malformed one) is likewise a
/// resolution failure.
// @cpt-algo:cpt-cf-oagw-algo-plugin-chain-assemble:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-chain-order:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-chain-concat:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-03
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-04
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-05
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-06
pub(crate) fn assemble_chain(
    auth_binding: Option<&PluginBinding>,
    upstream_bindings: &[PluginBinding],
    route_bindings: &[PluginBinding],
    registries: &Registries,
) -> Result<ExecutionPlan, BindingResolutionError> {
    let mut guards = Vec::new();
    let mut transforms = Vec::new();

    for binding in upstream_bindings.iter().chain(route_bindings.iter()) {
        match parse_plugin_identifier(&binding.plugin_ref, None) {
            Ok(PluginIdentifier::Named {
                plugin_type: PluginType::Guard,
                ..
            }) => guards.push(resolve_guard_binding(binding, registries)?),
            Ok(PluginIdentifier::Named {
                plugin_type: PluginType::Transform,
                ..
            }) => transforms.push(resolve_transform_binding(binding, registries)?),
            // An `auth_plugin` identifier at this position, a UUID-backed
            // binding, or a malformed candidate: all resolution failures.
            _ => {
                return Err(BindingResolutionError {
                    plugin_ref: binding.plugin_ref.clone(),
                    expected: PluginType::Guard,
                });
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-06
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-05
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-04
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-03
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-02
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-01

    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-07
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-08
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-09
    let auth = auth_binding
        .map(|binding| resolve_auth_binding(binding, registries))
        .transpose()?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-09
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-08
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-07

    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-10
    Ok(ExecutionPlan {
        auth,
        guards,
        transforms,
    })
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-assemble:p1:inst-chain-assemble-10
}

/// Test-only convenience: a guard binding with the given `config`.
#[cfg(test)]
pub(crate) fn guard_binding(token: &str, config: Value) -> PluginBinding {
    PluginBinding::new(
        crate::model::plugin::identity::named_plugin_gts_ref(PluginType::Guard, token),
        config,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::plugin::identity::named_plugin_gts_ref;
    use serde_json::json;

    fn registries() -> Registries {
        Registries::init()
    }

    #[test]
    fn empty_bindings_produce_an_empty_plan() {
        let plan = assemble_chain(None, &[], &[], &registries()).unwrap();
        assert!(plan.auth.is_none());
        assert!(plan.guards.is_empty());
        assert!(plan.transforms.is_empty());
    }

    #[test]
    fn upstream_bindings_are_concatenated_before_route_bindings() {
        let upstream = vec![
            guard_binding("required_headers", json!({"tag": "u1"})),
            guard_binding("required_headers", json!({"tag": "u2"})),
        ];
        let route = vec![guard_binding("required_headers", json!({"tag": "r1"}))];
        let plan = assemble_chain(None, &upstream, &route, &registries()).unwrap();
        let tags: Vec<_> = plan
            .guards
            .iter()
            .map(|g| g.config["tag"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(tags, vec!["u1", "u2", "r1"]);
    }

    #[test]
    fn the_same_identifier_bound_twice_runs_twice_with_its_own_config() {
        let upstream = vec![
            guard_binding("required_headers", json!({"required_request_headers": "a"})),
            guard_binding("required_headers", json!({"required_request_headers": "b"})),
        ];
        let plan = assemble_chain(None, &upstream, &[], &registries()).unwrap();
        assert_eq!(plan.guards.len(), 2);
    }

    #[test]
    fn resolution_failure_in_the_route_list_abandons_assembly() {
        let upstream = vec![guard_binding("required_headers", Value::Null)];
        let route = vec![PluginBinding::without_config(named_plugin_gts_ref(
            crate::model::plugin::PluginType::Guard,
            "timeout",
        ))];
        let err = assemble_chain(None, &upstream, &route, &registries()).unwrap_err();
        assert_eq!(
            err.plugin_ref,
            named_plugin_gts_ref(crate::model::plugin::PluginType::Guard, "timeout")
        );
    }

    #[test]
    fn transforms_and_guards_are_partitioned_by_their_own_kind() {
        let upstream = vec![
            guard_binding("required_headers", Value::Null),
            PluginBinding::without_config(named_plugin_gts_ref(
                crate::model::plugin::PluginType::Transform,
                "request_id",
            )),
        ];
        let plan = assemble_chain(None, &upstream, &[], &registries()).unwrap();
        assert_eq!(plan.guards.len(), 1);
        assert_eq!(plan.transforms.len(), 1);
    }

    #[test]
    fn auth_binding_is_resolved_independently_of_the_guard_transform_lists() {
        let auth = PluginBinding::without_config(named_plugin_gts_ref(
            crate::model::plugin::PluginType::Auth,
            "apikey",
        ));
        let plan = assemble_chain(Some(&auth), &[], &[], &registries()).unwrap();
        assert!(plan.auth.is_some());
    }

    #[test]
    fn no_auth_binding_is_distinct_from_noop_and_is_not_an_error() {
        let plan = assemble_chain(None, &[], &[], &registries()).unwrap();
        assert!(plan.auth.is_none());
    }

    #[test]
    fn an_auth_plugin_identifier_at_a_guard_position_is_a_resolution_failure() {
        let upstream = vec![PluginBinding::without_config(named_plugin_gts_ref(
            crate::model::plugin::PluginType::Auth,
            "noop",
        ))];
        assert!(assemble_chain(None, &upstream, &[], &registries()).is_err());
    }
}
