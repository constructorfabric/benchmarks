//! Plugin binding resolution (`cpt-cf-oagw-algo-plugin-binding-resolve`).
//!
//! Turns one classified `plugin_ref` binding into a resolved,
//! executable-kind entry paired with its `config`, or a resolution
//! failure carrying only the offending identifier -- never its `config`.
//! Identifier parsing and UUID-vs-named classification are reused
//! unchanged from `cpt-cf-oagw-feature-plugin-management`
//! ([`crate::model::plugin::identity`]); this module only decides
//! executability against the registries built by
//! [`super::registry::Registries`].

use serde_json::Value;

use crate::model::plugin::PluginType;
use crate::model::plugin::identity::{PluginIdentifier, parse_plugin_identifier};

use super::registry::{AuthKind, GuardKind, Registries, TransformKind};

/// One binding as handed to this feature by the merged effective
/// configuration: a `plugin_ref` (canonical GTS identifier string) plus
/// its `config` object. `cpt-cf-oagw-algo-plugin-chain-assemble`'s input
/// shape -- the richer entry point this feature's own functions consume
/// and that a follow-up round should widen `crate::proxy::engine`'s call
/// sites to produce (see the manifest note in `super::chain`).
#[derive(Debug, Clone)]
pub(crate) struct PluginBinding {
    pub plugin_ref: String,
    pub config: Value,
}

impl PluginBinding {
    #[must_use]
    pub(crate) fn new(plugin_ref: impl Into<String>, config: Value) -> Self {
        Self {
            plugin_ref: plugin_ref.into(),
            config,
        }
    }

    /// A binding with no reachable `config` -- the shape entry 2.5's
    /// current narrow `&[String]` call sites can produce (identifier only).
    #[must_use]
    pub(crate) fn without_config(plugin_ref: impl Into<String>) -> Self {
        Self::new(plugin_ref, Value::Null)
    }
}

/// A binding this feature could not resolve to an executable
/// implementation: a catalog-only identifier, an unknown one, a
/// kind-mismatched one, or a UUID-backed custom plugin (§5
/// `cpt-cf-oagw-dod-plugin-no-custom-execution`). Maps uniformly to `503
/// PluginNotFound` -- `cpt-cf-oagw-dod-plugin-binding-resolution`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingResolutionError {
    /// The offending identifier only -- never the binding's `config`.
    pub plugin_ref: String,
    /// The plugin kind expected at this position, for the kind-specific
    /// "unknown auth/guard/transform plugin" message
    /// (`inst-binding-resolve-07`).
    pub expected: PluginType,
}

#[derive(Debug)]
pub(crate) struct ResolvedAuth {
    pub kind: AuthKind,
    pub config: Value,
}

#[derive(Debug)]
pub(crate) struct ResolvedGuard {
    pub kind: GuardKind,
    pub config: Value,
}

#[derive(Debug)]
pub(crate) struct ResolvedTransform {
    pub kind: TransformKind,
    /// Carried for structural symmetry with `ResolvedAuth`/`ResolvedGuard`
    /// and for a future transform kind that needs configuration; the only
    /// registry-resolvable transform, `request_id`, is zero-config
    /// (`cpt-cf-oagw-algo-plugin-transform-apply`), so nothing reads this
    /// field today.
    #[allow(dead_code)]
    pub config: Value,
}

// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-04
// A resolution failure echoes only the offending identifier
// (`BindingResolutionError::plugin_ref`), never the binding's `config`.
fn unresolvable(plugin_ref: &str, expected: PluginType) -> BindingResolutionError {
    BindingResolutionError {
        plugin_ref: plugin_ref.to_owned(),
        expected,
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-04

/// Classify `plugin_ref` and confirm its base type matches `expected`
/// (`inst-binding-resolve-01`). A UUID-backed identifier -- bare or full
/// GTS form -- always fails closed regardless of kind
/// (`inst-binding-resolve-02`/`-03`): no execution engine for custom
/// Starlark plugin source exists this round.
// @cpt-algo:cpt-cf-oagw-algo-plugin-binding-resolve:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-03
// @cpt-dod:cpt-cf-oagw-dod-plugin-no-custom-execution:p2
fn classify_named(
    plugin_ref: &str,
    expected: PluginType,
) -> Result<String, BindingResolutionError> {
    match parse_plugin_identifier(plugin_ref, Some(expected)) {
        Ok(PluginIdentifier::Named { plugin_type, token }) if plugin_type == expected => Ok(token),
        // Any other outcome -- a kind mismatch, a UUID-backed binding
        // (custom plugin, unresolvable this round), or a malformed
        // candidate -- is a resolution failure.
        _ => Err(unresolvable(plugin_ref, expected)),
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-03
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-02
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-01

/// `inst-binding-resolve-05` through `-09`: look a named token up in the
/// registry for the expected kind, and return the resolved implementation
/// plus the binding's `config`, or a resolution failure.
// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-05
// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-06
// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-07
// @cpt-dod:cpt-cf-oagw-dod-plugin-catalog-only-ids:p2
// @cpt-dod:cpt-cf-oagw-dod-plugin-binding-resolution:p1
pub(crate) fn resolve_auth_binding(
    binding: &PluginBinding,
    registries: &Registries,
) -> Result<ResolvedAuth, BindingResolutionError> {
    let token = classify_named(&binding.plugin_ref, PluginType::Auth)?;
    let kind = registries
        .auth(&token)
        .ok_or_else(|| unresolvable(&binding.plugin_ref, PluginType::Auth))?;
    // @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-09
    Ok(ResolvedAuth {
        kind,
        config: binding.config.clone(),
    })
    // @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-09
}

pub(crate) fn resolve_guard_binding(
    binding: &PluginBinding,
    registries: &Registries,
) -> Result<ResolvedGuard, BindingResolutionError> {
    let token = classify_named(&binding.plugin_ref, PluginType::Guard)?;
    let kind = registries
        .guard(&token)
        .ok_or_else(|| unresolvable(&binding.plugin_ref, PluginType::Guard))?;
    Ok(ResolvedGuard {
        kind,
        config: binding.config.clone(),
    })
}

pub(crate) fn resolve_transform_binding(
    binding: &PluginBinding,
    registries: &Registries,
) -> Result<ResolvedTransform, BindingResolutionError> {
    let token = classify_named(&binding.plugin_ref, PluginType::Transform)?;
    let kind = registries
        .transform(&token)
        .ok_or_else(|| unresolvable(&binding.plugin_ref, PluginType::Transform))?;
    Ok(ResolvedTransform {
        kind,
        config: binding.config.clone(),
    })
}
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-07
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-06
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-05

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::plugin::identity::{named_plugin_gts_ref, plugin_gts_ref};
    use uuid::Uuid;

    fn registries() -> Registries {
        Registries::init()
    }

    #[test]
    fn resolves_a_backed_named_auth_token() {
        let binding = PluginBinding::without_config(named_plugin_gts_ref(PluginType::Auth, "noop"));
        let resolved = resolve_auth_binding(&binding, &registries()).unwrap();
        assert_eq!(resolved.kind, AuthKind::Noop);
    }

    #[test]
    fn catalog_only_token_fails_to_bind() {
        let binding =
            PluginBinding::without_config(named_plugin_gts_ref(PluginType::Auth, "basic"));
        let err = resolve_auth_binding(&binding, &registries()).unwrap_err();
        assert_eq!(
            err.plugin_ref,
            named_plugin_gts_ref(PluginType::Auth, "basic")
        );
        assert_eq!(err.expected, PluginType::Auth);
    }

    #[test]
    fn catalog_only_guard_identifiers_fail_to_bind() {
        for token in ["timeout", "cors"] {
            let binding =
                PluginBinding::without_config(named_plugin_gts_ref(PluginType::Guard, token));
            assert!(resolve_guard_binding(&binding, &registries()).is_err());
        }
    }

    #[test]
    fn catalog_only_transform_identifiers_fail_to_bind() {
        for token in ["logging", "metrics"] {
            let binding =
                PluginBinding::without_config(named_plugin_gts_ref(PluginType::Transform, token));
            assert!(resolve_transform_binding(&binding, &registries()).is_err());
        }
    }

    #[test]
    fn kind_mismatch_fails_to_bind() {
        // An auth_plugin identifier bound at a guard position.
        let binding = PluginBinding::without_config(named_plugin_gts_ref(PluginType::Auth, "noop"));
        let err = resolve_guard_binding(&binding, &registries()).unwrap_err();
        assert_eq!(err.expected, PluginType::Guard);
    }

    #[test]
    fn uuid_backed_binding_fails_closed_even_with_matching_kind() {
        let uuid = Uuid::new_v4();
        let binding = PluginBinding::without_config(plugin_gts_ref(PluginType::Guard, uuid));
        let err = resolve_guard_binding(&binding, &registries()).unwrap_err();
        assert_eq!(err.plugin_ref, plugin_gts_ref(PluginType::Guard, uuid));
    }

    #[test]
    fn bare_uuid_binding_fails_closed() {
        let uuid = Uuid::new_v4();
        let binding = PluginBinding::without_config(uuid.to_string());
        assert!(resolve_transform_binding(&binding, &registries()).is_err());
    }

    #[test]
    fn unknown_named_identifier_fails_to_bind() {
        let binding = PluginBinding::without_config(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.frobnicate.v1",
        );
        assert!(resolve_auth_binding(&binding, &registries()).is_err());
    }

    #[test]
    fn resolves_both_oauth2_client_credential_variants_distinctly() {
        let form = PluginBinding::without_config(named_plugin_gts_ref(
            PluginType::Auth,
            "oauth2_client_cred",
        ));
        let basic = PluginBinding::without_config(named_plugin_gts_ref(
            PluginType::Auth,
            "oauth2_client_cred_basic",
        ));
        assert_eq!(
            resolve_auth_binding(&form, &registries()).unwrap().kind,
            AuthKind::OAuth2ClientCred
        );
        assert_eq!(
            resolve_auth_binding(&basic, &registries()).unwrap().kind,
            AuthKind::OAuth2ClientCredBasic
        );
    }
}
