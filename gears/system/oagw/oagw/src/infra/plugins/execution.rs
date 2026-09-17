//! The plugin execution engine of the data plane
//! ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md) "Execution Order").
//!
//! [`PluginExecution`] resolves the chain of one request and runs it in the
//! order the ADR fixes:
//!
//! ```text
//! Auth → Guards → Transform(request) → upstream call → Guards → Transform(response)
//!                                                                    └→ Transform(error)
//! ```
//!
//! Upstream-bound plugins run before route-bound ones at every stage, a guard
//! rejection short-circuits the stage, and the engine never talks to a
//! transport: the upstream call is a [`UpstreamCall`] port the proxy implements,
//! so the same chain runs against any request/response view.
//!
//! A guard runs on the response path too, before the response transforms — the
//! `required_headers` guard checks the upstream's response headers before a
//! transform mutates them. The error transforms run on every error the chain
//! produces, whatever stage rejected.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderMap;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{AuthConfig, PluginBinding};

use super::{
    AuthPlugin, ErrorView, GuardDecision, GuardPlugin, PluginCatalog, PluginInstance,
    PluginRegistry, RequestContext, TransformPlugin, UpstreamResponseView,
};

/// One resolved plugin of a chain, together with its binding's configuration.
struct Bound<P: ?Sized> {
    plugin: Arc<P>,
    plugin_ref: String,
    config: Option<Value>,
}

impl<P: ?Sized> Bound<P> {
    /// Publishes the binding's identity and configuration on the request
    /// context, so the plugin reads its own configuration from `ctx.config`.
    fn prepare(&self, ctx: &mut RequestContext) {
        ctx.plugin_ref.clone_from(&self.plugin_ref);
        ctx.config.clone_from(&self.config);
    }
}

impl<P: ?Sized> fmt::Debug for Bound<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bound")
            .field("plugin_ref", &self.plugin_ref)
            .field("config", &self.config)
            .finish()
    }
}

/// The resolved plugin chain of one request.
///
/// [`PluginExecution::resolve`] decides the order once, so the data plane cannot
/// compose it differently: the upstream's `auth` plugin first, then the guards,
/// then the transforms — upstream-bound before route-bound within each stage.
#[derive(Debug, Default)]
pub struct PluginExecution {
    auth: Option<Bound<dyn AuthPlugin>>,
    guards: Vec<Bound<dyn GuardPlugin>>,
    transforms: Vec<Bound<dyn TransformPlugin>>,
}

impl PluginExecution {
    /// Resolves the plugin chain of one request against `registry`.
    ///
    /// A `plugins.items` binding carries no configuration of its own in this
    /// build's model, so a resolved guard or transform runs unconfigured.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when a binding names an unknown plugin or a
    /// catalog-only identifier, or when a second auth plugin is bound;
    /// [`OagwError::PluginNotFound`] when a bound plugin has no in-process
    /// implementation.
    pub fn resolve(
        registry: &PluginRegistry,
        custom: Option<&PluginCatalog>,
        tenant_id: Uuid,
        upstream_auth: Option<&AuthConfig>,
        upstream: &[PluginBinding],
        route: &[PluginBinding],
    ) -> Result<Self, OagwError> {
        let mut chain = Self::default();
        if let Some(auth_type) = upstream_auth.and_then(|auth| auth.auth_type.as_ref()) {
            let plugin = registry.auth_plugin(auth_type.as_str()).ok_or_else(|| {
                OagwError::PluginNotFound {
                    message: format!("auth plugin '{}' is not registered", auth_type.as_str()),
                }
            })?;
            chain.auth = Some(Bound {
                plugin,
                plugin_ref: auth_type.as_str().to_owned(),
                config: upstream_auth.and_then(|auth| auth.config.clone()),
            });
        }
        for (scope, bindings) in [("upstream", upstream), ("route", route)] {
            for binding in bindings {
                let resolved = resolve_binding(registry, custom, tenant_id, binding, scope)?;
                match resolved {
                    ResolvedPlugin::Auth(plugin_ref) => {
                        return Err(OagwError::Validation {
                            message: format!(
                                "plugin '{plugin_ref}' is an auth plugin: only one auth plugin \
                                 binds per resource, through 'auth.type' rather than \
                                 'plugins.items'"
                            ),
                        });
                    }
                    ResolvedPlugin::Guard(plugin, config) => {
                        let plugin_ref = plugin.id().to_owned();
                        chain.guards.push(Bound {
                            plugin,
                            plugin_ref,
                            config,
                        });
                    }
                    ResolvedPlugin::Transform(plugin, config) => {
                        let plugin_ref = plugin.id().to_owned();
                        chain.transforms.push(Bound {
                            plugin,
                            plugin_ref,
                            config,
                        });
                    }
                }
            }
        }
        Ok(chain)
    }

    /// `true` when no plugin is bound at all, so the chain is a no-op.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.auth.is_none() && self.guards.is_empty() && self.transforms.is_empty()
    }

    /// Runs the request side of the chain: Auth → Guards → Transform.
    ///
    /// A guard rejection stops the chain: no later guard, no request transform
    /// and no upstream call.
    ///
    /// # Errors
    /// Whatever the first failing plugin reports, which aborts the chain.
    pub async fn run_request(&self, ctx: &mut RequestContext) -> Result<GuardDecision, OagwError> {
        if let Some(bound) = &self.auth {
            bound.prepare(ctx);
            bound.plugin.authenticate(ctx).await?;
        }
        for bound in &self.guards {
            bound.prepare(ctx);
            let decision = bound.plugin.guard_request(ctx).await?;
            if !decision.is_allow() {
                return Ok(decision);
            }
        }
        for bound in &self.transforms {
            bound.prepare(ctx);
            bound.plugin.transform_request(ctx).await?;
        }
        Ok(GuardDecision::Allow)
    }

    /// Runs the response side of the chain: Guards → Transform, so a guard sees
    /// the upstream's response before a transform mutates it.
    ///
    /// # Errors
    /// Whatever the first failing plugin reports.
    pub async fn run_response(
        &self,
        ctx: &mut RequestContext,
        response: &mut UpstreamResponseView,
    ) -> Result<GuardDecision, OagwError> {
        for bound in &self.guards {
            bound.prepare(ctx);
            let decision = bound.plugin.guard_response(ctx, response).await?;
            if !decision.is_allow() {
                return Ok(decision);
            }
        }
        for bound in &self.transforms {
            bound.prepare(ctx);
            bound.plugin.transform_response(ctx, response).await?;
        }
        Ok(GuardDecision::Allow)
    }

    /// Runs the error transforms on `error`.
    ///
    /// # Errors
    /// Whatever the first failing transform reports.
    pub async fn run_error(
        &self,
        ctx: &mut RequestContext,
        error: &mut ErrorView,
    ) -> Result<(), OagwError> {
        for bound in &self.transforms {
            bound.prepare(ctx);
            bound.plugin.transform_error(ctx, error).await?;
        }
        Ok(())
    }

    /// Runs the whole chain around `upstream`, the transport-independent port
    /// the proxy implements: the request side, the upstream call, then the
    /// response side.
    ///
    /// A guard rejection in either phase and a failed upstream call both come
    /// back as [`ExecutionOutcome::Error`], after the error transforms ran on
    /// the error view.
    ///
    /// # Errors
    /// Whatever a plugin raises and cannot recover from.
    pub async fn execute(
        &self,
        ctx: &mut RequestContext,
        upstream: &dyn UpstreamCall,
    ) -> Result<ExecutionOutcome, OagwError> {
        if let Some(error) = rejection_of(self.run_request(ctx).await?) {
            return self.rejected(ctx, error).await;
        }
        match upstream.send(ctx).await {
            Ok(mut response) => match rejection_of(self.run_response(ctx, &mut response).await?) {
                Some(error) => self.rejected(ctx, error).await,
                None => Ok(ExecutionOutcome::Response(response)),
            },
            Err(error) => self.rejected(ctx, error).await,
        }
    }

    /// Runs the error transforms on `error` and reports it as the outcome.
    async fn rejected(
        &self,
        ctx: &mut RequestContext,
        mut error: ErrorView,
    ) -> Result<ExecutionOutcome, OagwError> {
        self.run_error(ctx, &mut error).await?;
        Ok(ExecutionOutcome::Error(error))
    }
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;

/// The error view a guard rejection is reported as, or `None` when it allowed.
fn rejection_of(decision: GuardDecision) -> Option<ErrorView> {
    match decision {
        GuardDecision::Allow => None,
        GuardDecision::Reject {
            status,
            error_code,
            message,
        } => Some(ErrorView {
            status,
            error_code,
            message,
            headers: HeaderMap::new(),
        }),
    }
}

/// The upstream call the engine drives
/// ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md)): the transport half
/// of the proxy, which the engine knows nothing about.
#[async_trait]
pub trait UpstreamCall: Send + Sync {
    /// Performs the upstream request. A failure is reported as the error view
    /// the error transforms run on.
    ///
    /// # Errors
    /// A failed upstream call is an [`ErrorView`], not an `Err`: the engine runs
    /// the error transforms on it either way.
    async fn send(&self, ctx: &mut RequestContext) -> Result<UpstreamResponseView, ErrorView>;
}

/// What one proxied request ended as.
#[derive(Debug)]
pub enum ExecutionOutcome {
    /// The upstream responded and the chain let the response through.
    Response(UpstreamResponseView),
    /// A guard or the upstream call produced the error response.
    Error(ErrorView),
}

/// A plugin a `plugins.items` binding resolved to, before it is filed under its
/// stage.
enum ResolvedPlugin {
    Auth(String),
    Guard(Arc<dyn GuardPlugin>, Option<Value>),
    Transform(Arc<dyn TransformPlugin>, Option<Value>),
}

/// Resolves one `plugins.items` binding into the plugin it names.
///
/// The catalog-only identifiers of the PRD (`timeout`, `cors`, `logging`,
/// `metrics`) are core Data Plane logic and never executable, so binding one is
/// a 400 rather than a silent no-op.
fn resolve_binding(
    registry: &PluginRegistry,
    custom: Option<&PluginCatalog>,
    tenant_id: Uuid,
    binding: &PluginBinding,
    scope: &str,
) -> Result<ResolvedPlugin, OagwError> {
    let reference = binding.as_str();
    if let Some(descriptor) = registry.catalog().descriptor_for(reference)
        && !descriptor.bindable
    {
        return Err(OagwError::Validation {
            message: format!(
                "plugin '{reference}' is a catalog-only identifier and cannot be bound as a {scope} \
                 plugin"
            ),
        });
    }
    match registry.resolve(reference) {
        Some(PluginInstance::Auth(..)) => Ok(ResolvedPlugin::Auth(
            registry
                .catalog()
                .canonical_id(reference)
                .map_or_else(|| reference.to_owned(), str::to_owned),
        )),
        Some(PluginInstance::Guard(plugin)) => Ok(ResolvedPlugin::Guard(plugin, None)),
        Some(PluginInstance::Transform(plugin)) => Ok(ResolvedPlugin::Transform(plugin, None)),
        None => Err(unknown_plugin(binding, custom, tenant_id, reference)),
    }
}

/// The error a binding that resolved to nothing is reported as: 503 for a custom
/// plugin the tenant owns but that has no implementation, 400 for everything
/// else.
fn unknown_plugin(
    binding: &PluginBinding,
    custom: Option<&PluginCatalog>,
    tenant_id: Uuid,
    reference: &str,
) -> OagwError {
    let custom_id = binding.as_uuid();
    let owned = custom_id
        .is_some_and(|id| custom.is_some_and(|catalog| catalog.get(tenant_id, id).is_some()));
    if owned {
        return OagwError::PluginNotFound {
            message: format!("custom plugin '{reference}' has no in-process implementation"),
        };
    }
    if custom_id.is_some() {
        return OagwError::Validation {
            message: format!("plugin '{reference}' does not exist in the calling tenant"),
        };
    }
    OagwError::Validation {
        message: format!("plugin '{reference}' is not a known plugin"),
    }
}
