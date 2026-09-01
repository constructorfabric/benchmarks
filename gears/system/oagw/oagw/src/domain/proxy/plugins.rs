// Created: 2026-08-31 by Constructor Tech
//! The plugin chain of one proxied request (DESIGN §3.2 "Plugin System").
//!
//! # Composition (documented choice)
//!
//! The upstream is the ancestor configuration level, the route the descendant
//! one. DESIGN §3.2 fixes both the composition and the order of the two:
//! "upstream plugins execute before route plugins
//! (`[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`)", so the effective chain is the
//! **concatenation** of the two, in that order:
//!
//! | Upstream `plugins.sharing` | Route declares a chain | Effective chain |
//! |---|---|---|
//! | any | no | the upstream chain |
//! | any | yes | upstream chain + route chain, in this order |
//!
//! The upstream `plugins.sharing` mode is **not** given the power to let a route
//! drop an upstream binding, which is a deliberate reading of the two
//! references. `SharingMode` (PRD §5.5) governs visibility across the *tenant*
//! hierarchy — the ancestor an upstream is inherited from — and the data plane
//! already applies it there. Reading it as "a descendant may override" would let
//! a route silently switch off an upstream guard, which is the one direction
//! this slice never opens: an upstream that requires a signed request must not
//! lose that requirement because a route bound its own guard.
//!
//! # Resolution
//!
//! A chain is resolved **per request**, against the live registries. A reference
//! that resolves to no implementation is a 503 `plugin.not_found.v1`, never a
//! silent skip: a chain that cannot be enforced in full must not run in part.
//!
//! # Order
//!
//! `AuthPlugin` → guards → `TransformPlugin::transform_request` → upstream call
//! → `TransformPlugin::transform_response` on success, `transform_error` on a
//! gateway failure (DESIGN §3.2 "Execution Order").

use std::sync::Arc;

use crate::domain::model::{PluginBinding, PluginsConfig, Route, Upstream};
use crate::domain::plugin::PluginRef;
use crate::error::OagwResult;
use crate::infra::plugin::PluginRegistries;
use crate::infra::plugin::registry::{unresolved_auth_plugin, unresolved_plugin};
use crate::infra::plugin::traits::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PluginConfig, RequestContext,
    ResponseContext, TransformPlugin, UpstreamRef,
};

/// The resolved chain of one request.
///
/// Every entry carries the configuration it was bound with; a plugin only ever
/// sees its own.
pub struct PluginChain {
    /// Auth binding of the upstream; at most one.
    pub auth: Option<AuthBinding>,
    /// Guard plugins, upstream before route.
    pub guards: Vec<(Arc<dyn GuardPlugin>, PluginConfig)>,
    /// Transform plugins, upstream before route.
    pub transforms: Vec<(Arc<dyn TransformPlugin>, PluginConfig)>,
}

/// The auth plugin of one request plus its configuration.
pub struct AuthBinding {
    /// The plugin.
    pub plugin: Arc<dyn AuthPlugin>,
    /// Configuration from the `auth` binding.
    pub config: PluginConfig,
}

impl PluginChain {
    /// Whether the chain carries no plugin at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.auth.is_none() && self.guards.is_empty() && self.transforms.is_empty()
    }
}

/// Compose the upstream and the route chains (see the module docs).
#[must_use]
pub fn merge_chains(
    upstream: Option<&PluginsConfig>,
    route: Option<&PluginsConfig>,
) -> Vec<PluginBinding> {
    let Some(upstream) = upstream else {
        return route.map_or_else(Vec::new, |chain| chain.items.clone());
    };
    let Some(route) = route else {
        return upstream.items.clone();
    };
    let mut merged = upstream.items.clone();
    merged.extend(route.items.iter().cloned());
    merged
}

/// Resolve the chain of one request against the live registries.
///
/// # Errors
/// 503 `plugin.not_found.v1` for a chain reference that resolves to no
/// implementation, 503 `link.unavailable.v1` for an auth binding this
/// deployment cannot honour.
pub fn resolve_chain(
    upstream: &Upstream,
    route: Option<&Route>,
    registries: &PluginRegistries,
    store: &dyn crate::domain::store::Store,
) -> OagwResult<PluginChain> {
    let tenant_id = upstream.tenant_id;
    let auth = match upstream.auth.as_ref() {
        Some(binding) => {
            // A present `auth` member is a promise: the upstream is only
            // dialled with its credentials. A `type` that names nothing (and a
            // missing plugin, below) is a link that is not available, never a
            // silent forward without credentials.
            let Some(reference) = binding
                .plugin_type
                .as_deref()
                .filter(|reference| !reference.trim().is_empty())
            else {
                return Err(unresolved_auth_plugin("type"));
            };
            Some(AuthBinding {
                plugin: registries
                    .auth()
                    .get(reference)
                    .ok_or_else(|| unresolvable_auth(reference))?,
                config: auth_plugin_config(&binding.raw),
            })
        }
        None => None,
    };
    let bindings = merge_chains(
        upstream.plugins.as_ref(),
        route.and_then(|r| r.plugins.as_ref()),
    );
    let mut guards = Vec::new();
    let mut transforms = Vec::new();
    for binding in bindings {
        let reference = binding.reference().to_owned();
        let config = binding
            .config()
            .map_or_else(PluginConfig::empty, |map| PluginConfig::new(map.clone()));
        match resolve_reference(&reference, tenant_id, registries, store)? {
            Resolved::Guard(plugin) => guards.push((plugin, config)),
            Resolved::Transform(plugin) => transforms.push((plugin, config)),
        }
    }
    Ok(PluginChain {
        auth,
        guards,
        transforms,
    })
}

/// Configuration of an auth binding for its plugin.
///
/// The upstream schema nests the plugin members under `config`
/// (ADR-0008 "Upstream Configuration Example"); a flat binding
/// (`"auth": { "type": …, "key_ref": … }`) is accepted as well, which is what
/// the write path has always validated.
#[must_use]
pub fn auth_plugin_config(raw: &serde_json::Map<String, serde_json::Value>) -> PluginConfig {
    match raw.get("config") {
        Some(serde_json::Value::Object(nested)) => PluginConfig::new(nested.clone()),
        _ => PluginConfig::new(raw.clone()),
    }
}

/// One resolved chain entry.
enum Resolved {
    Guard(Arc<dyn GuardPlugin>),
    Transform(Arc<dyn TransformPlugin>),
}

/// Resolve one chain reference, failing closed.
fn resolve_reference(
    reference: &str,
    tenant_id: uuid::Uuid,
    registries: &PluginRegistries,
    store: &dyn crate::domain::store::Store,
) -> OagwResult<Resolved> {
    match PluginRef::parse(reference) {
        PluginRef::BuiltIn { kind, .. } => resolve_built_in(kind, reference, registries),
        PluginRef::Custom { .. } => resolve_custom(reference, tenant_id, registries, store),
        // Neither a GTS id nor a UUID: the only remaining legal spelling is a
        // bare built-in name (`apikey`), which the catalog classifies.
        PluginRef::Unrecognised(_) => {
            resolve_short_name(reference, registries).ok_or_else(|| unresolved_plugin(reference))
        }
    }
}

/// Resolve a bare built-in name against the family registry the catalog puts
/// it in.
fn resolve_short_name(reference: &str, registries: &PluginRegistries) -> Option<Resolved> {
    let plugin = crate::domain::plugin::lookup_built_in_by_name(reference)
        .filter(|built_in| built_in.resolvable)?;
    match plugin.kind {
        crate::domain::model::PluginKind::Guard => {
            registries.guard().get(reference).map(Resolved::Guard)
        }
        crate::domain::model::PluginKind::Transform => registries
            .transform()
            .get(reference)
            .map(Resolved::Transform),
        // An auth plugin is never bound through the chain: `upstream.auth`
        // carries it.
        crate::domain::model::PluginKind::Auth => None,
    }
}

/// Resolve a built-in reference against its family registry.
fn resolve_built_in(
    kind: crate::domain::model::PluginKind,
    reference: &str,
    registries: &PluginRegistries,
) -> OagwResult<Resolved> {
    let resolved = match kind {
        crate::domain::model::PluginKind::Guard => {
            registries.guard().get(reference).map(Resolved::Guard)
        }
        crate::domain::model::PluginKind::Transform => registries
            .transform()
            .get(reference)
            .map(Resolved::Transform),
        // An auth plugin is never bound through the chain: `upstream.auth`
        // carries it.
        crate::domain::model::PluginKind::Auth => None,
    };
    resolved.ok_or_else(|| unresolved_plugin(reference))
}

/// Resolve a custom reference, which must exist and must have an
/// implementation.
///
/// Custom plugins are Starlark records; this slice ships no interpreter, so a
/// record that exists is still not executable and fails closed with the same
/// 503 an unknown reference gets (see the module docs).
fn resolve_custom(
    reference: &str,
    tenant_id: uuid::Uuid,
    registries: &PluginRegistries,
    store: &dyn crate::domain::store::Store,
) -> OagwResult<Resolved> {
    let parsed = PluginRef::parse(reference);
    let Some(id) = parsed.custom_id() else {
        return Err(unresolved_plugin(reference));
    };
    let Some(record) = store.get_plugin(tenant_id, id)? else {
        return Err(unresolved_plugin(reference));
    };
    let resolved = match record.kind {
        crate::domain::model::PluginKind::Guard => registries
            .guard()
            .get(&record.gts_id())
            .map(Resolved::Guard),
        crate::domain::model::PluginKind::Transform => registries
            .transform()
            .get(&record.gts_id())
            .map(Resolved::Transform),
        crate::domain::model::PluginKind::Auth => None,
    };
    resolved.ok_or_else(|| unresolved_plugin(reference))
}

/// 503 for an auth binding the data plane cannot honour.
///
/// A catalogued built-in without an implementation (`basic`, `bearer`) is a
/// binding that cannot exist at all; a *resolvable* built-in that is missing
/// from the registry means the credential store is not wired — either way the
/// request is not forwarded without its credentials.
fn unresolvable_auth(reference: &str) -> crate::error::OagwError {
    if crate::domain::plugin::is_unbindable_built_in(reference) {
        return unresolved_plugin(reference);
    }
    unresolved_auth_plugin(reference)
}

/// Run the request-side half of the chain (DESIGN §3.2 "Execution Order").
///
/// # Errors
/// A guard rejection, or a failure of the auth or transform phase.
pub async fn run_request_phase(chain: &PluginChain, ctx: &mut RequestContext) -> OagwResult<()> {
    if let Some(binding) = &chain.auth {
        ctx.config = binding.config.clone();
        binding.plugin.authenticate(ctx).await?;
    }
    for (guard, config) in &chain.guards {
        ctx.config = config.clone();
        if let GuardDecision::Reject(rejection) = guard.guard_request(ctx).await? {
            return Err(rejection.into_error());
        }
    }
    for (transform, config) in &chain.transforms {
        ctx.config = config.clone();
        transform.transform_request(ctx).await?;
    }
    Ok(())
}

/// Run the response-side half of the chain.
///
/// # Errors
/// A guard rejection or a transform failure, while the upstream head is still
/// uncommitted — a rejection here is a 502 the client actually sees.
pub async fn run_response_phase(chain: &PluginChain, ctx: &mut ResponseContext) -> OagwResult<()> {
    for (guard, config) in &chain.guards {
        ctx.config = config.clone();
        if let GuardDecision::Reject(rejection) = guard.guard_response(ctx).await? {
            return Err(rejection.into_error());
        }
    }
    for (transform, config) in &chain.transforms {
        ctx.config = config.clone();
        transform.transform_response(ctx).await?;
    }
    Ok(())
}

/// Run the error-side half of the chain over the failure the gateway is about
/// to render.
///
/// A transform may enrich the **extensions** of the problem (a correlation id,
/// a `Retry-After` hint, the plugin it relates to); the classification and the
/// detail the gateway decided on are restored afterwards, so the status and the
/// problem type are never a plugin's to change, and a failing transform never
/// replaces the client's failure with its own — the original one is reported.
pub async fn run_error_phase(chain: &PluginChain, ctx: &mut ErrorContext) {
    let decided = ctx.error.clone();
    for (transform, config) in &chain.transforms {
        ctx.config = config.clone();
        if transform.transform_error(ctx).await.is_err() {
            tracing::warn!(
                plugin = transform.id(),
                "error-phase transform failed; the gateway problem is reported unchanged"
            );
            break;
        }
    }
    let transformed = std::mem::replace(&mut ctx.error, decided.clone());
    let extensions = transformed.extensions().clone();
    ctx.error =
        decided.with_extension(move |extensions_of_error| *extensions_of_error = extensions);
}

/// Build the per-request identity reference.
#[must_use]
pub fn upstream_ref(id: uuid::Uuid, alias: &str) -> UpstreamRef {
    UpstreamRef {
        id,
        alias: alias.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{OagwError, OagwErrorKind};

    /// A transform that records the error it saw and then rewrites it.
    struct Recording {
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl TransformPlugin for Recording {
        fn id(&self) -> &'static str {
            "recording"
        }

        fn plugin_type(&self) -> &'static str {
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.recording.v1"
        }

        async fn transform_request(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
            Ok(())
        }

        async fn transform_response(&self, _ctx: &mut ResponseContext) -> Result<(), OagwError> {
            Ok(())
        }

        async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(ctx.error.detail().to_owned());
            }
            if self.fail {
                return Err(OagwError::new(
                    OagwErrorKind::Internal,
                    "the transform failed",
                ));
            }
            ctx.error = OagwError::new(OagwErrorKind::AliasConflict, "the plugin decided")
                .with_extension(|extensions| {
                    extensions.invalid_value = Some("from-the-plugin".to_owned());
                });
            Ok(())
        }
    }

    fn context(detail: &str) -> ErrorContext {
        // The builder can only fail on missing ids; both are set.
        let security = toolkit_security::SecurityContext::builder()
            .subject_id(uuid::Uuid::now_v7())
            .subject_tenant_id(uuid::Uuid::now_v7())
            .build()
            .unwrap_or_else(|_| toolkit_security::SecurityContext::anonymous());
        ErrorContext {
            security,
            upstream: upstream_ref(uuid::Uuid::now_v7(), "api.vendor.com"),
            error: OagwError::new(OagwErrorKind::UnknownTargetHost, detail),
            config: PluginConfig::empty(),
        }
    }

    fn chain_of(transform: Arc<dyn TransformPlugin>) -> PluginChain {
        PluginChain {
            auth: None,
            guards: Vec::new(),
            transforms: Vec::from([(transform, PluginConfig::empty())]),
        }
    }

    /// What a `Recording` observed, shared with the test.
    type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn recording(fail: bool) -> (Arc<Recording>, Seen) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let plugin = Arc::new(Recording {
            seen: std::sync::Arc::clone(&seen),
            fail,
        });
        (plugin, seen)
    }

    #[tokio::test]
    async fn the_error_phase_sees_the_gateway_failure() {
        let (plugin, seen) = recording(false);
        let mut ctx = context("assembled upstream URL is not a valid URL");

        run_error_phase(&chain_of(plugin), &mut ctx).await;

        // The hook observed the failure the gateway decided on …
        assert_eq!(
            seen.lock().map_or_else(|_| Vec::new(), |seen| seen.clone()),
            Vec::from(["assembled upstream URL is not a valid URL".to_owned()])
        );
        // … but the classification is still the gateway's, and the extension
        // the hook set reached the problem document.
        assert_eq!(*ctx.error.kind(), OagwErrorKind::UnknownTargetHost);
        assert_eq!(
            ctx.error.detail(),
            "assembled upstream URL is not a valid URL"
        );
        assert_eq!(
            ctx.error.extensions().invalid_value.as_deref(),
            Some("from-the-plugin")
        );
    }

    #[tokio::test]
    async fn a_failing_transform_never_replaces_the_gateway_failure() {
        let (plugin, seen) = recording(true);
        let mut ctx = context("the credential store is unavailable");

        run_error_phase(&chain_of(plugin), &mut ctx).await;

        assert_eq!(
            seen.lock().map_or_else(|_| Vec::new(), |seen| seen.clone()),
            Vec::from(["the credential store is unavailable".to_owned()])
        );
        assert_eq!(*ctx.error.kind(), OagwErrorKind::UnknownTargetHost);
        assert_eq!(ctx.error.detail(), "the credential store is unavailable");
        assert_eq!(ctx.error.extensions().invalid_value, None);
    }
}
