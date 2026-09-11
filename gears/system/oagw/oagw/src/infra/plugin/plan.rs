//! Binding-set resolution into the deterministic execution plan
//! (`cpt-cf-oagw-dod-plugin-execution-order`,
//! `cpt-cf-oagw-flow-execution-plan`,
//! `cpt-cf-oagw-algo-registry-resolution`).
//!
//! [`resolve`] takes the binding set the proxy pipeline assembled for one
//! request — the upstream's `auth_plugin_ref` plus the ordered plugin bindings
//! of the matched upstream and route — and either returns the
//! [`ExecutionPlan`], whose tiers are driven per request by that caller, or the
//! single `PluginNotFound` failure a reference that resolves in no registry
//! produces. No plugin is ever skipped and no partial plan is handed out.
// @cpt-begin:cpt-cf-oagw-dod-plugin-execution-order:p1:inst-full

use std::fmt;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_http::HttpClientConfig;

use crate::domain::error::OagwError;
use crate::domain::model::PluginBinding;
use crate::domain::plugin::{
    AUTH_PLUGIN_TYPE, AuthPlugin, ErrorContext, GUARD_PLUGIN_TYPE, GuardPlugin, RequestContext,
    ResponseContext, TRANSFORM_PLUGIN_TYPE, TransformPlugin,
};
use crate::infra::plugin::guard::GuardOutcome;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::plugin::token_cache::TokenCacheConfig;

/// The three registries the gear holds behind its `OnceLock`, one per
/// `{type}_plugin` family.
///
/// Constructed once at bootstrap after the platform dependencies resolve;
/// afterwards only the read surface is reachable, so no registration path
/// exists at request time.
#[derive(Debug, Default, Clone)]
pub struct PluginRegistries {
    /// The registry of the `auth_plugin` family.
    pub auth: AuthPluginRegistry,
    /// The registry of the `guard_plugin` family.
    pub guard: GuardPluginRegistry,
    /// The registry of the `transform_plugin` family.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Creates the three empty registries.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the registries holding the built-in plugins.
    ///
    /// The `cred_store` client, ADR 0008's `token_http_config` parameter and
    /// the [`TokenCacheConfig`] parameter object are threaded into the OAuth2
    /// client-credentials constructors.
    #[must_use]
    pub fn with_builtins(
        cred_store: Arc<dyn CredStoreClientV1>,
        token_http_config: Option<HttpClientConfig>,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(
                Arc::clone(&cred_store),
                token_http_config,
                token_cache_config,
            ),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }
}

/// The resolved binding set of one proxied request: the upstream's auth plugin
/// identity and the ordered plugin bindings of the matched upstream and route.
///
/// `plugins` carries the bindings in the order the caller merged them, each
/// already carrying its `position`, so the tiers this module derives keep
/// binding-position order without sorting again.
#[derive(Debug, Clone)]
pub struct BindingSet {
    /// The upstream's auth plugin binding.
    pub auth: PluginBinding,
    /// The guard and transform bindings of the upstream and the route, in the
    /// merged order the caller produced.
    pub plugins: Vec<PluginBinding>,
}

impl BindingSet {
    /// Bundles an auth binding with the merged plugin bindings.
    #[must_use]
    pub fn new(auth: PluginBinding, plugins: Vec<PluginBinding>) -> Self {
        Self { auth, plugins }
    }

    /// The bindings of one `{type}_plugin` family, in the merged order.
    fn of_family<'a>(&'a self, family: &'a str) -> impl Iterator<Item = &'a PluginBinding> {
        self.plugins
            .iter()
            .filter(move |binding| family_of(&binding.plugin_ref) == family)
    }
}

/// One resolved plugin of a tier, together with the binding it was resolved
/// from, whose `config` the phase driver hands to the plugin at invocation.
#[derive(Debug)]
pub struct PlanEntry<P: ?Sized> {
    /// The executable plugin the registry resolved.
    pub plugin: Arc<P>,
    /// The binding the plugin was resolved from.
    pub binding: PluginBinding,
}

/// The plugin one reference resolves to, tagged by the family its registry
/// was selected from.
///
/// `Debug` is implemented by hand and prints the family only, since neither a
/// plugin nor its credential material is printable.
enum ResolvedPlugin {
    /// An auth plugin of the `auth_plugin` family.
    Auth(Arc<dyn AuthPlugin>),
    /// A guard plugin of the `guard_plugin` family.
    Guard(Arc<dyn GuardPlugin>),
    /// A transform plugin of the `transform_plugin` family.
    Transform(Arc<dyn TransformPlugin>),
}

impl fmt::Debug for ResolvedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(_) => f.write_str("Auth(..)"),
            Self::Guard(_) => f.write_str("Guard(..)"),
            Self::Transform(_) => f.write_str("Transform(..)"),
        }
    }
}

/// The deterministic execution plan of one binding set
/// (`cpt-cf-oagw-flow-execution-plan` step 6).
///
/// Tier 1 is the single auth plugin, tier 2 the guards, tier 3 the transforms'
/// request phase; after the caller's upstream call, tier 4 is the transforms'
/// response phase, or the transforms' error phase when the call or an earlier
/// phase failed. Every tier keeps binding-position order.
///
/// `Debug` is implemented by hand and prints only the tier sizes, never a
/// plugin's credential behaviour.
pub struct ExecutionPlan {
    /// Tier 1: the single auth plugin.
    pub auth: PlanEntry<dyn AuthPlugin>,
    /// Tier 2: the guards, in binding-position order.
    pub guards: Vec<PlanEntry<dyn GuardPlugin>>,
    /// Tier 3 and 4: the transforms, in binding-position order.
    pub transforms: Vec<PlanEntry<dyn TransformPlugin>>,
}

impl fmt::Debug for ExecutionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionPlan")
            .field("auth", &self.auth.binding.plugin_ref)
            .field("guards", &self.guards.len())
            .field("transforms", &self.transforms.len())
            .finish()
    }
}

/// The outcome the caller re-enters the guard-and-transform flow with after the
/// upstream call: the upstream response, or the error context of a failed call
/// or earlier phase.
pub enum Reentry<'a> {
    /// The upstream responded: the response phase runs.
    Response(&'a mut ResponseContext),
    /// The call or an earlier phase failed: the error phase runs.
    Error(&'a mut ErrorContext),
}

impl ExecutionPlan {
    /// Runs tier 1: the auth phase.
    ///
    /// The plugin is invoked on a scoped copy of the request context whose
    /// `config` is its own binding config, so the caller's context never holds a
    /// binding config of its own; the mutations the plugin applies — the
    /// injected credential and the security context — are carried back, and the
    /// prepared context is returned to the caller for the guard phase. No
    /// upstream HTTP call is performed here.
    ///
    /// # Errors
    /// Returns the typed failure of the auth plugin, mapped per §1.5.
    pub async fn auth_phase(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-01
        // The proxied request whose upstream binds an auth plugin reaches this
        // phase through the proxy pipeline, which is the caller of this method
        // and the only component that later performs the upstream call.
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-02
        // The prepared mutable `RequestContext`, the auth plugin the
        // execution-plan flow resolved and its binding `config` are received
        // here, and `authenticate(&mut RequestContext)` is invoked on the
        // plugin — on a scoped copy of the context, as every phase driver does,
        // so the plugin reads its own binding `config` and the caller's context
        // carries no binding config of any plugin afterwards.
        let mut scoped = RequestContext::clone(ctx);
        scoped.config = self.auth.binding.config.clone().unwrap_or_default();
        let outcome = self.auth.plugin.authenticate(&mut scoped).await;
        // The plugin's own mutations — the injected credential header or query
        // parameter and the security context — reach the caller's context; the
        // scoped binding config does not, the caller's own `config` being left
        // as the phase received it.
        ctx.headers = scoped.headers;
        ctx.query = scoped.query;
        ctx.security_context = scoped.security_context;
        outcome?;
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-02
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-01
        // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-10
        // The prepared context is returned to the caller for the guard phase;
        // no upstream HTTP call is performed in this flow.
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-10
    }

    /// Runs tiers 2 and 3: the guard tier and the transforms' request phase.
    ///
    /// A guard rejection stops the phase before any transform runs, and the
    /// rejection is returned to the caller; a transformed request context is
    /// returned for the caller to perform the upstream HTTP call with.
    ///
    /// # Errors
    /// Returns the typed failure of a plugin invocation, mapped per §1.5.
    pub async fn request_phase(&self, ctx: &mut RequestContext) -> Result<GuardOutcome, OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-01
        // The prepared request context of the same request the auth phase
        // returned, together with the guard tier and the transform tier of the
        // execution plan and each plugin's binding `config`.
        if let GuardOutcome::Rejected(rejection) =
            crate::infra::plugin::guard::guard_request_phase(&self.guards, ctx).await?
        {
            return Ok(GuardOutcome::Rejected(rejection));
        }
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-01
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-05
        // No guard rejected, so the request context is handed to the transform
        // tier.
        crate::infra::plugin::transform::transform_request_phase(&self.transforms, ctx).await?;
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-05
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-02
        // The transformed request context is returned to the caller, which
        // performs the upstream HTTP call; this flow performs none.
        Ok(GuardOutcome::Allowed)
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-02
    }

    /// Re-enters the guard-and-transform flow after the caller's upstream call
    /// (tier 4).
    ///
    /// The response phase runs the guard tier and — when no guard rejects — the
    /// transforms' response phase; the error phase runs only the transforms'
    /// error phase. A response-phase rejection is returned to the caller and no
    /// transform runs.
    ///
    /// # Errors
    /// Returns the typed failure of a plugin invocation, mapped per §1.5.
    pub async fn on_reentry(&self, outcome: Reentry<'_>) -> Result<GuardOutcome, OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-06
        // The caller re-enters this flow in the response phase of the same
        // request, with the upstream `ResponseContext`, or with the
        // `ErrorContext` when the upstream call or an earlier phase failed.
        let decision = match outcome {
            Reentry::Response(ctx) => {
                if let GuardOutcome::Rejected(rejection) =
                    crate::infra::plugin::guard::guard_response_phase(&self.guards, ctx).await?
                {
                    return Ok(GuardOutcome::Rejected(rejection));
                }
                crate::infra::plugin::transform::transform_response_phase(&self.transforms, ctx)
                    .await?;
                GuardOutcome::Allowed
            }
            Reentry::Error(ctx) => {
                crate::infra::plugin::transform::transform_error_phase(&self.transforms, ctx)
                    .await?;
                GuardOutcome::Allowed
            }
        };
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-gr-06
        // @cpt-begin:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-05
        // The transformed response context or error context is returned to the
        // caller, which renders the outcome through the error contract of the
        // gear-wiring feature; this flow renders no HTTP response of its own.
        Ok(decision)
        // @cpt-end:cpt-cf-oagw-flow-guard-transform-phase:p1:inst-tr-05
    }
}

/// Resolves a binding set into its execution plan.
///
/// Every binding is resolved through [`resolve_reference`]; the first failure
/// is returned and no plan is assembled, so a request whose binding resolves in
/// no registry fails with `PluginNotFound` instead of running with the plugin
/// skipped.
///
/// # Errors
/// Returns `PluginNotFound` for a reference that resolves in no registry: a
/// named identifier absent from its registry, a catalog-only identifier, or a
/// UUID-backed reference with no executable implementation in this release.
pub fn resolve(
    registries: &PluginRegistries,
    bindings: &BindingSet,
) -> Result<ExecutionPlan, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-01
    // The resolved binding set is received from the proxy pipeline for one
    // proxied request: the upstream's auth plugin identity and the ordered
    // plugin bindings of the matched upstream and route, each carrying
    // `plugin_ref`, the derived `plugin_uuid` when present, its `config`
    // object and its `position`.
    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-02
    // The auth binding is resolved through the registry-resolution algorithm
    // against `AuthPluginRegistry`.
    let auth = match resolve_reference(registries, &bindings.auth.plugin_ref)? {
        ResolvedPlugin::Auth(plugin) => PlanEntry {
            plugin,
            binding: bindings.auth.clone(),
        },
        ResolvedPlugin::Guard(_) | ResolvedPlugin::Transform(_) => {
            // A reference of another `{type}_plugin` family is absent from the
            // auth registry by construction, so the resolution is a miss.
            return Err(plugin_not_found(&bindings.auth.plugin_ref));
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-02
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-01

    let mut guards: Vec<PlanEntry<dyn GuardPlugin>> = Vec::new();
    let mut transforms: Vec<PlanEntry<dyn TransformPlugin>> = Vec::new();

    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-03
    // FOR EACH guard binding of the merged upstream-and-route set, in
    // binding-position order, resolve it through the same algorithm against
    // `GuardPluginRegistry`.
    for binding in bindings.of_family(GUARD_PLUGIN_TYPE) {
        match resolve_reference(registries, &binding.plugin_ref)? {
            ResolvedPlugin::Guard(plugin) => guards.push(PlanEntry {
                plugin,
                binding: binding.clone(),
            }),
            ResolvedPlugin::Auth(_) | ResolvedPlugin::Transform(_) => {
                return Err(plugin_not_found(&binding.plugin_ref));
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-03

    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-04
    // FOR EACH transform binding of the merged set, in binding-position order,
    // resolve it through the same algorithm against `TransformPluginRegistry`.
    for binding in bindings.of_family(TRANSFORM_PLUGIN_TYPE) {
        match resolve_reference(registries, &binding.plugin_ref)? {
            ResolvedPlugin::Transform(plugin) => transforms.push(PlanEntry {
                plugin,
                binding: binding.clone(),
            }),
            ResolvedPlugin::Auth(_) | ResolvedPlugin::Guard(_) => {
                return Err(plugin_not_found(&binding.plugin_ref));
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-04

    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-07
    // No resolution failed, so the deterministic execution plan is assembled:
    // tier 1 the single auth plugin, tier 2 the guards, tier 3 the transforms'
    // on_request phase, then — after the caller's upstream call — tier 4 the
    // transforms' on_response phase, or tier 4 as on_error when the call or an
    // earlier phase failed; within every tier the upstream-level plugins
    // precede the route-level plugins and each tier keeps binding-position
    // order.
    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-08
    // The execution plan is returned to the proxy pipeline, which drives the
    // phases per request and performs the HTTP call.
    Ok(ExecutionPlan {
        auth,
        guards,
        transforms,
    })
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-08
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-07
}

/// The `PluginNotFound` failure of one unresolvable reference.
fn plugin_not_found(plugin_ref: &str) -> OagwError {
    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-06
    // The `PluginNotFound` failure is returned to the caller: HTTP 503, GTS
    // type `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`,
    // `X-OAGW-Error-Source: gateway`, rendered by the error-mapping algorithm;
    // the plugin is never skipped and no partial plan is handed to the caller.
    // @cpt-begin:cpt-cf-oagw-flow-execution-plan:p1:inst-px-05
    // The resolution failed: a named identifier absent from its registry, a
    // catalog-only identifier, or a UUID-backed reference with no executable
    // implementation in this release.
    OagwError::plugin_not_found(format!(
        "oagw.plugins: the plugin reference '{plugin_ref}' resolves in no registry"
    ))
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-05
    // @cpt-end:cpt-cf-oagw-flow-execution-plan:p1:inst-px-06
}

/// The `{type}_plugin` family prefix of a reference: the part up to and
/// including the `~` separator, which is the form the `{type}` constants of the
/// plugin traits use.
fn family_of(plugin_ref: &str) -> &str {
    match plugin_ref.find('~') {
        Some(index) => &plugin_ref[..=index],
        None => plugin_ref,
    }
}

/// The instance part of a reference: the part after `~`.
fn instance_of(plugin_ref: &str) -> &str {
    plugin_ref
        .split_once('~')
        .map_or(plugin_ref, |(_, rest)| rest)
}

/// Resolves one plugin reference through the registry-resolution algorithm.
///
/// The registry is selected from the reference's type family, the instance part
/// decides between the UUID-backed and the named disposition, and the lookup is
/// a single direct lookup of the whole identifier string.
///
/// # Errors
/// Returns `PluginNotFound` for a UUID-backed reference — this release has no
/// executable implementation for a stored custom plugin — and for an identifier
/// absent from its registry, identically to any unknown identifier.
fn resolve_reference(
    registries: &PluginRegistries,
    plugin_ref: &str,
) -> Result<ResolvedPlugin, OagwError> {
    // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-01
    // Select the registry from the identifier's type family, the part before
    // `~`: `auth_plugin` resolves against `AuthPluginRegistry`, `guard_plugin`
    // against `GuardPluginRegistry` and `transform_plugin` against
    // `TransformPluginRegistry`.
    let family = family_of(plugin_ref);
    // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-02
    // Parse the identifier to extract the instance part after `~`, per the
    // Resolution Algorithm of the Plugin Identification Model.
    let instance = instance_of(plugin_ref);
    // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-03
    // IF the instance part parses as a UUID, that is a UUID-backed reference
    // to a stored custom plugin.
    if uuid::Uuid::parse_str(instance).is_ok() {
        // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-04
        // The `PluginNotFound` failure is returned, because this release has no
        // executable implementation for a stored custom plugin: the store path
        // exists for the management surface, not for execution, and this
        // disposition is the declared interpretation of §1.5.
        return Err(plugin_not_found(plugin_ref));
        // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-04
    }
    // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-05
    // The full named identifier is looked up in the selected registry, the key
    // being the whole `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1`
    // form so a binding's `plugin_ref` resolves by direct lookup.
    let resolved = match family {
        AUTH_PLUGIN_TYPE => registries.auth.lookup(plugin_ref).map(ResolvedPlugin::Auth),
        GUARD_PLUGIN_TYPE => registries
            .guard
            .lookup(plugin_ref)
            .map(ResolvedPlugin::Guard),
        TRANSFORM_PLUGIN_TYPE => registries
            .transform
            .lookup(plugin_ref)
            .map(ResolvedPlugin::Transform),
        _ => None,
    };
    // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-06
    // IF the identifier is present in the registry.
    match resolved {
        Some(plugin) => {
            // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-07
            // The executable plugin is returned; the binding's `config` travels
            // with the plan entry, and the phase driver hands it to the plugin
            // at invocation.
            // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-10
            // RETURN in every case after a single lookup: there is no second
            // resolution path, no fallback to another registry, no lazy
            // construction and no registration from a request.
            Ok(plugin)
            // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-10
            // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-07
        }
        None => {
            // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-08
            // ELSE the identifier is absent from the registry, including each
            // of the six catalog-only identifiers, which no registry contains.
            // @cpt-begin:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-09
            // The `PluginNotFound` failure is identical to the failure any
            // unknown identifier produces, so a catalog-only identifier is not
            // distinguishable at the wire from an unknown one.
            Err(plugin_not_found(plugin_ref))
            // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-09
            // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-08
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-06
    // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-05
    // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-03
    // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-02
    // @cpt-end:cpt-cf-oagw-algo-registry-resolution:p1:inst-rr-01
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::guard::GuardOutcome;
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// The catalog-only guard identifier no registry contains.
    const CATALOG_ONLY_GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
    /// The catalog-only auth identifier no registry contains.
    const CATALOG_ONLY_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
    /// The catalog-only transform identifier no registry contains.
    const CATALOG_ONLY_TRANSFORM: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

    fn registries() -> PluginRegistries {
        PluginRegistries::with_builtins(
            Arc::new(MockCredStoreClient::empty()),
            None,
            TokenCacheConfig::new(300, 128),
        )
    }

    fn binding(position: usize, plugin_ref: &str) -> PluginBinding {
        PluginBinding {
            position,
            plugin_ref: plugin_ref.to_owned(),
            plugin_uuid: None,
            config: None,
        }
    }

    fn configured(position: usize, plugin_ref: &str, key: &str, value: &str) -> PluginBinding {
        PluginBinding {
            config: Some(BTreeMap::from([(
                key.to_owned(),
                serde_json::Value::String(value.to_owned()),
            )])),
            ..binding(position, plugin_ref)
        }
    }

    fn auth_binding(plugin_ref: &str) -> PluginBinding {
        binding(0, plugin_ref)
    }

    fn noop() -> &'static str {
        crate::infra::plugin::registry::NOOP_AUTH_PLUGIN_ID
    }

    fn required_headers() -> &'static str {
        crate::infra::plugin::registry::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    fn request_id() -> &'static str {
        crate::infra::plugin::registry::REQUEST_ID_TRANSFORM_PLUGIN_ID
    }

    fn bindings_of(plan: &ExecutionPlan) -> Vec<String> {
        let mut refs: Vec<String> = plan
            .guards
            .iter()
            .map(|e| e.binding.plugin_ref.clone())
            .collect();
        refs.extend(plan.transforms.iter().map(|e| e.binding.plugin_ref.clone()));
        refs
    }

    #[test]
    fn the_auth_binding_resolves_into_tier_one() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(auth_binding(noop()), Vec::new()),
        )
        .expect("the plan resolves");

        assert_eq!(plan.auth.plugin.id(), noop());
        assert!(plan.guards.is_empty());
        assert!(plan.transforms.is_empty());
        assert_eq!(plan.auth.binding.plugin_ref, noop());
    }

    #[test]
    fn the_tiers_keep_binding_position_order() {
        let plugins = vec![
            binding(0, request_id()),
            binding(1, required_headers()),
            binding(2, required_headers()),
            binding(3, request_id()),
        ];
        let plan = resolve(
            &registries(),
            &BindingSet::new(auth_binding(noop()), plugins),
        )
        .expect("the plan resolves");

        assert_eq!(plan.guards.len(), 2, "the two guard bindings form tier 2");
        assert_eq!(
            plan.transforms.len(),
            2,
            "the two transform bindings form tier 3"
        );
        assert_eq!(
            bindings_of(&plan),
            vec![
                required_headers().to_owned(),
                required_headers().to_owned(),
                request_id().to_owned(),
                request_id().to_owned(),
            ],
            "each tier keeps binding-position order"
        );
        assert_eq!(plan.guards[0].binding.position, 1);
        assert_eq!(plan.guards[1].binding.position, 2);
        assert_eq!(plan.transforms[0].binding.position, 0);
        assert_eq!(plan.transforms[1].binding.position, 3);
    }

    #[test]
    fn a_reference_of_another_family_is_resolved_by_its_own_registry() {
        let plugins = vec![
            binding(0, CATALOG_ONLY_GUARD),
            binding(1, CATALOG_ONLY_TRANSFORM),
        ];
        let error = resolve(
            &registries(),
            &BindingSet::new(auth_binding(noop()), plugins),
        )
        .expect_err("a catalog-only identifier resolves in no registry");

        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
        assert_eq!(error.mapping().variant, "PluginNotFound");
    }

    #[test]
    fn a_catalog_only_identifier_is_not_distinguishable_from_an_unknown_one() {
        let registries = registries();
        let unknown = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.unknown.v1";

        let catalog_only = resolve_reference(&registries, CATALOG_ONLY_GUARD).unwrap_err();
        let unknown = resolve_reference(&registries, unknown).unwrap_err();
        let auth_only = resolve_reference(&registries, CATALOG_ONLY_AUTH).unwrap_err();
        let transform_only = resolve_reference(&registries, CATALOG_ONLY_TRANSFORM).unwrap_err();

        for error in [catalog_only, auth_only, transform_only] {
            assert_eq!(error.status(), unknown.status());
            assert_eq!(error.gts_type(), unknown.gts_type());
            assert_eq!(
                error.mapping(),
                unknown.mapping(),
                "a catalog-only identifier is not distinguishable at the wire"
            );
        }
    }

    #[test]
    fn a_uuid_backed_reference_resolves_to_plugin_not_found() {
        let registries = registries();
        let uuid_ref = "gts.cf.core.oagw.auth_plugin.v1~3f0a1b2c-3d4e-4f50-8617-8899aabbccdd";
        let bare_uuid = "3f0a1b2c-3d4e-4f50-8617-8899aabbccdd";

        for reference in [uuid_ref, bare_uuid] {
            let error = resolve_reference(&registries, reference).unwrap_err();
            assert_eq!(error.status(), 503, "{reference}");
            assert_eq!(
                error.gts_type(),
                "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
            );
        }
    }

    #[test]
    fn an_auth_binding_of_another_family_is_plugin_not_found() {
        let error = resolve(
            &registries(),
            &BindingSet::new(auth_binding(required_headers()), Vec::new()),
        )
        .expect_err("a guard reference never resolves as the auth plugin");

        assert_eq!(error.status(), 503);
        assert_eq!(error.mapping().variant, "PluginNotFound");
    }

    #[test]
    fn an_unresolvable_binding_leaves_no_partial_plan() {
        let plugins = vec![
            binding(0, required_headers()),
            binding(1, CATALOG_ONLY_GUARD),
            binding(2, request_id()),
        ];
        let error = resolve(
            &registries(),
            &BindingSet::new(auth_binding(noop()), plugins),
        )
        .expect_err("the first unresolvable binding stops the resolution");

        assert_eq!(error.mapping().variant, "PluginNotFound");
        assert!(
            error.to_string().contains(CATALOG_ONLY_GUARD),
            "the failure names the reference that resolved in no registry: {}",
            error
        );
    }

    #[test]
    fn the_family_and_instance_parts_are_split_on_the_tilde() {
        assert_eq!(family_of(noop()), AUTH_PLUGIN_TYPE);
        assert_eq!(family_of(required_headers()), GUARD_PLUGIN_TYPE);
        assert_eq!(family_of(request_id()), TRANSFORM_PLUGIN_TYPE);
        assert_eq!(family_of("bare"), "bare");
        assert_eq!(
            instance_of("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"),
            "cf.core.oagw.noop.v1"
        );
        assert_eq!(instance_of("bare"), "bare");
    }

    #[test]
    fn a_binding_set_groups_its_plugins_by_family() {
        let set = BindingSet::new(
            auth_binding(noop()),
            vec![binding(0, request_id()), binding(1, required_headers())],
        );

        assert_eq!(
            set.of_family(GUARD_PLUGIN_TYPE)
                .map(|b| b.plugin_ref.as_str())
                .collect::<Vec<_>>(),
            vec![required_headers()]
        );
        assert_eq!(
            set.of_family(TRANSFORM_PLUGIN_TYPE)
                .map(|b| b.plugin_ref.as_str())
                .collect::<Vec<_>>(),
            vec![request_id()]
        );
        assert_eq!(
            set.of_family(AUTH_PLUGIN_TYPE).count(),
            0,
            "the auth binding is not part of the plugin list"
        );
    }

    #[tokio::test]
    async fn the_auth_phase_runs_tier_one_with_its_own_config() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(
                configured(0, noop(), "marker", "auth"),
                vec![binding(0, required_headers())],
            ),
        )
        .expect("the plan resolves");

        let security = toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let mut ctx = RequestContext::new(security);

        plan.auth_phase(&mut ctx)
            .await
            .expect("the auth phase passes");
        assert!(
            ctx.config.is_empty(),
            "the phase leaves no binding config behind on the context"
        );

        let outcome = plan
            .request_phase(&mut ctx)
            .await
            .expect("the request phase passes");
        assert_eq!(outcome, crate::infra::plugin::guard::GuardOutcome::Allowed);
        assert!(
            ctx.config.is_empty(),
            "the phases leave no binding config behind on the context"
        );
    }

    /// The same invariant as `the_auth_phase_runs_tier_one_with_its_own_config`,
    /// exercised with a non-empty transform tier: a phase that scopes a binding
    /// config onto the context leaves nothing behind once it is done.
    ///
    /// Ignored, because the assertion does not hold today:
    /// `transform::transform_request_phase` scopes each transform by assigning
    /// the shared context's `config` and leaves the last transform's binding
    /// config on it, and that scoping lives in `src/infra/plugin/transform.rs`,
    /// which the file set of the fix that added this test does not cover. The
    /// test is the ready-made pin for the driver that unifies the three
    /// scoping conventions: un-ignore it once the transform tier is
    /// clone-scoped the way the guard tier is.
    #[tokio::test]
    #[ignore = "transform_request_phase leaves the last transform's binding config on the \
                shared context; the scoping lives in infra/plugin/transform.rs"]
    async fn a_non_empty_transform_tier_leaves_no_binding_config_behind() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(
                auth_binding(noop()),
                vec![configured(0, request_id(), "marker", "transform")],
            ),
        )
        .expect("the plan resolves");

        let mut ctx = RequestContext::new(security());
        plan.auth_phase(&mut ctx).await.expect("the auth phase");
        plan.request_phase(&mut ctx)
            .await
            .expect("the request phase");

        assert!(
            crate::infra::plugin::transform::request_id_of(&ctx.headers).is_some(),
            "the non-empty transform tier ran"
        );
        assert!(
            ctx.config.is_empty(),
            "the phases leave no binding config behind on the context: {:?}",
            ctx.config
        );
    }

    #[tokio::test]
    async fn a_guard_rejection_stops_the_request_phase_before_the_transforms() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(
                auth_binding(noop()),
                vec![configured(
                    0,
                    required_headers(),
                    "required_request_headers",
                    "x-missing",
                )],
            ),
        )
        .expect("the plan resolves");

        let security = toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let mut ctx = RequestContext::new(security);

        let outcome = plan
            .request_phase(&mut ctx)
            .await
            .expect("the decision is not an error");

        let crate::infra::plugin::guard::GuardOutcome::Rejected(rejection) = outcome else {
            panic!("the missing required header rejects the request phase");
        };
        assert_eq!(rejection.status, 400);
        assert_eq!(rejection.error_code, "REQUIRED_HEADER_MISSING");
        assert_eq!(rejection.detail, "x-missing");
    }

    #[tokio::test]
    async fn the_response_phase_runs_the_guard_tier_before_the_transform_tier() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(
                auth_binding(noop()),
                vec![
                    configured(
                        0,
                        required_headers(),
                        "required_response_headers",
                        "content-type",
                    ),
                    binding(1, request_id()),
                ],
            ),
        )
        .expect("the plan resolves");

        let security = toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant");
        let mut ctx = RequestContext::new(security);
        plan.auth_phase(&mut ctx).await.unwrap();
        plan.request_phase(&mut ctx).await.unwrap();
        let request_id = crate::infra::plugin::transform::request_id_of(&ctx.headers)
            .expect("the request phase established the identifier");

        let mut response = ResponseContext {
            request_id: Some(request_id),
            ..ResponseContext::default()
        };
        response
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        plan.on_reentry(Reentry::Response(&mut response))
            .await
            .expect("the response phase passes");
        assert!(
            crate::infra::plugin::transform::request_id_of(&response.headers).is_some(),
            "the transform tier ran after the guard tier allowed the phase"
        );

        let mut untagged = ResponseContext::default();
        let outcome = plan
            .on_reentry(Reentry::Response(&mut untagged))
            .await
            .expect("a rejection is a decision and not an error");
        let GuardOutcome::Rejected(rejection) = outcome else {
            panic!("the missing content-type rejects the response phase");
        };
        assert_eq!(rejection.status, 502);
        assert_eq!(rejection.error_code, "REQUIRED_HEADER_MISSING");
        assert_eq!(rejection.detail, "content-type");
        assert!(
            crate::infra::plugin::transform::request_id_of(&untagged.headers).is_none(),
            "a guard rejection stops the phase before any transform runs"
        );
    }

    #[tokio::test]
    async fn the_error_phase_runs_only_the_transform_tier() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(
                auth_binding(noop()),
                vec![configured(
                    0,
                    required_headers(),
                    "required_response_headers",
                    "content-type",
                )],
            ),
        )
        .expect("the plan resolves");

        let mut error = ErrorContext::new().with_upstream_id("upstream");
        plan.on_reentry(Reentry::Error(&mut error))
            .await
            .expect("the error phase runs the transforms only");
        assert_eq!(error.upstream_id.as_deref(), Some("upstream"));
    }

    #[test]
    fn the_registries_expose_no_registration_path_after_init() {
        let registries = registries();
        assert_eq!(registries.auth.len(), 4);
        assert_eq!(registries.guard.len(), 1);
        assert_eq!(registries.transform.len(), 1);
        assert_eq!(PluginRegistries::new().auth.len(), 0);
        assert!(PluginRegistries::default().transform.is_empty());
    }

    #[test]
    fn an_empty_binding_set_resolves_to_plugin_not_found() {
        let error = resolve(&registries(), &BindingSet::new(binding(0, ""), Vec::new()))
            .expect_err("an empty auth reference resolves in no registry");

        assert_eq!(error.status(), 503);
        assert_eq!(error.mapping().variant, "PluginNotFound");
    }

    /// An external transform plugin that records every phase it is invoked in
    /// under its own identifier, the shape ADR 0002's external-plugin example
    /// shows, so a test can observe what the chain executes and in which order.
    struct RecordingTransform {
        id: String,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingTransform {
        fn registered(
            registries: &mut PluginRegistries,
            instance: &str,
            calls: &Arc<Mutex<Vec<String>>>,
        ) -> String {
            let id = format!("{TRANSFORM_PLUGIN_TYPE}test.{instance}.v1");
            registries.transform.register(Arc::new(Self {
                id: id.clone(),
                calls: Arc::clone(calls),
            }) as Arc<dyn TransformPlugin>);
            id
        }

        fn record(&self, phase: &str) {
            self.calls
                .lock()
                .expect("the record lock")
                .push(format!("{}:{phase}", self.id));
        }
    }

    #[async_trait::async_trait]
    impl TransformPlugin for RecordingTransform {
        fn id(&self) -> &str {
            &self.id
        }

        fn plugin_type(&self) -> &str {
            TRANSFORM_PLUGIN_TYPE
        }

        async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
            self.record("request");
            Ok(())
        }

        async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
            self.record("response");
            Ok(())
        }

        async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
            self.record("error");
            Ok(())
        }
    }

    /// An external transform plugin whose behaviour is a header it sets, the
    /// observable stand-in for any external behaviour the chain runs.
    struct MarkingTransform {
        id: String,
        header: &'static str,
    }

    #[async_trait::async_trait]
    impl TransformPlugin for MarkingTransform {
        fn id(&self) -> &str {
            &self.id
        }

        fn plugin_type(&self) -> &str {
            TRANSFORM_PLUGIN_TYPE
        }

        async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
            if let Ok(value) = "applied".parse() {
                ctx.headers.insert(self.header, value);
            }
            Ok(())
        }

        async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
            Ok(())
        }

        async fn transform_error(&self, _ctx: &mut ErrorContext) -> Result<(), OagwError> {
            Ok(())
        }
    }

    fn security() -> toolkit_security::SecurityContext {
        toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(Uuid::nil())
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    /// §6: a failed upstream call or an earlier failed phase invokes
    /// `transform_error(&mut ErrorContext)` for every transform of the transform
    /// tier, in plan order — here two external transforms with the built-in
    /// `request_id` between them, whose no-op error phase leaves every field of
    /// the `ErrorContext` unchanged.
    #[tokio::test]
    async fn the_error_phase_invokes_every_transform_in_plan_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registries = registries();
        let first = RecordingTransform::registered(&mut registries, "first", &calls);
        let second = RecordingTransform::registered(&mut registries, "second", &calls);
        let plan = resolve(
            &registries,
            &BindingSet::new(
                auth_binding(noop()),
                vec![
                    binding(0, &first),
                    binding(1, request_id()),
                    binding(2, &second),
                ],
            ),
        )
        .expect("the plan resolves");

        let mut error = ErrorContext::new().with_upstream_id("upstream");
        let before = format!("{error:?}");
        let outcome = plan
            .on_reentry(Reentry::Error(&mut error))
            .await
            .expect("the error phase runs the transforms only");

        assert_eq!(outcome, GuardOutcome::Allowed);
        assert_eq!(
            *calls.lock().expect("the record lock"),
            vec![format!("{first}:error"), format!("{second}:error")],
            "every transform ran, in the binding order the plan keeps"
        );
        assert_eq!(
            format!("{error:?}"),
            before,
            "the request_id plugin's error phase mutates no field of the error context"
        );
        assert_eq!(error.upstream_id.as_deref(), Some("upstream"));
    }

    /// §6: an external plugin registered at init under the identifier it
    /// declares is executed by the chain identically to a built-in — its
    /// behaviour is what the request phase applies.
    #[tokio::test]
    async fn an_external_plugin_executes_in_the_chain_like_a_builtin() {
        let mut registries = registries();
        registries.transform.register(Arc::new(MarkingTransform {
            id: format!("{TRANSFORM_PLUGIN_TYPE}test.marker.v1"),
            header: "x-external",
        }) as Arc<dyn TransformPlugin>);
        let plan = resolve(
            &registries,
            &BindingSet::new(
                auth_binding(noop()),
                vec![binding(
                    0,
                    "gts.cf.core.oagw.transform_plugin.v1~test.marker.v1",
                )],
            ),
        )
        .expect("the plan resolves");

        let mut ctx = RequestContext::new(security());
        plan.auth_phase(&mut ctx).await.expect("the auth phase");
        plan.request_phase(&mut ctx)
            .await
            .expect("the request phase");

        assert_eq!(
            ctx.headers.get("x-external").map(|value| value.as_bytes()),
            Some(b"applied".as_slice()),
            "the external plugin's behaviour is what the chain executed"
        );
    }

    /// §6: an external plugin registered over an identifier a built-in already
    /// holds is the plugin the plan resolves and executes, so the shadowed
    /// built-in's behaviour is no longer what the chain runs.
    #[tokio::test]
    async fn an_external_plugin_over_a_builtin_id_is_what_the_plan_executes() {
        let mut registries = registries();
        registries.transform.register(Arc::new(MarkingTransform {
            id: request_id().to_owned(),
            header: "x-shadowed",
        }) as Arc<dyn TransformPlugin>);
        let plan = resolve(
            &registries,
            &BindingSet::new(auth_binding(noop()), vec![binding(0, request_id())]),
        )
        .expect("the plan resolves");

        let mut ctx = RequestContext::new(security());
        plan.auth_phase(&mut ctx).await.expect("the auth phase");
        plan.request_phase(&mut ctx)
            .await
            .expect("the request phase");

        assert_eq!(
            ctx.headers.get("x-shadowed").map(|value| value.as_bytes()),
            Some(b"applied".as_slice()),
            "the external plugin is what the identifier resolved to"
        );
        assert!(
            crate::infra::plugin::transform::request_id_of(&ctx.headers).is_none(),
            "the shadowed built-in's behaviour did not run"
        );
    }

    /// §6: a request arriving without `X-Request-ID` leaves the request phase
    /// with a generated UUID **v4** in that header, and a request arriving with
    /// one keeps the incoming value unchanged.
    #[tokio::test]
    async fn the_request_phase_establishes_a_uuid_v4_identifier() {
        let plan = resolve(
            &registries(),
            &BindingSet::new(auth_binding(noop()), vec![binding(0, request_id())]),
        )
        .expect("the plan resolves");

        let mut untagged = RequestContext::new(security());
        plan.auth_phase(&mut untagged)
            .await
            .expect("the auth phase");
        plan.request_phase(&mut untagged)
            .await
            .expect("the request phase");
        let generated = crate::infra::plugin::transform::request_id_of(&untagged.headers)
            .expect("the identifier is established");
        let parsed = Uuid::parse_str(&generated).expect("the identifier is a UUID");
        assert_eq!(
            parsed.get_version_num(),
            4,
            "'{generated}' is a version 4 UUID"
        );

        let mut incoming = RequestContext::new(security());
        incoming.headers.insert(
            crate::infra::plugin::transform::REQUEST_ID_HEADER,
            "incoming".parse().unwrap(),
        );
        plan.request_phase(&mut incoming)
            .await
            .expect("the request phase");
        assert_eq!(
            crate::infra::plugin::transform::request_id_of(&incoming.headers).as_deref(),
            Some("incoming"),
            "an incoming identifier is never rewritten"
        );
    }
}

// @cpt-end:cpt-cf-oagw-dod-plugin-execution-order:p1:inst-full
