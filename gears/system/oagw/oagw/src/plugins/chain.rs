//! Plugin chain composition and execution order.
//!
//! Realizes `cpt-cf-oagw-algo-chain-compose`: the per-phase sub-chains the
//! effective plugin binding sets compose to, in the order DESIGN §3.2 Plugin
//! System states — auth, then guards, then transforms on the request, then the
//! upstream call, then guards and transforms on the response, then the error
//! transform when the call fails. Within a phase the upstream layer's
//! positions run before the route layer's, and within a layer the stored
//! position decides, so `[U1, U2] + [R1, R2]` composes to `[U1, U2, R1, R2]`.
//!
//! The composition composes only the binding sets it is given: the
//! cross-layer concatenation of an ancestor's and a descendant's set is the
//! merge `cpt-cf-oagw-feature-hierarchical-config` performs, and it is never
//! re-derived here. Nothing here runs a plugin — the composed chain is the
//! schedule the data plane executes, and a custom binding is carried as the
//! persisted row whose source that execution runs.

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::control_plane::binding::{self, ResolvedPlugin};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin::Plugin;
use crate::domain::plugin_contract::{PluginFamily, PluginPhase};
use crate::plugins::PluginRegistries;
use crate::store::PluginBinding;

/// The upstream's one auth plugin, as the composition resolved it from the
/// scalar identity columns.
#[derive(Clone)]
pub enum ComposedAuth {
    /// The upstream binds no auth plugin, which resolves to the no-op
    /// behaviour: the phase runs and injects nothing.
    Noop,
    /// A built-in auth implementation the registry holds.
    Builtin {
        /// The implementation the identifier resolved to.
        plugin: Arc<dyn crate::domain::plugin_contract::AuthPlugin>,
        /// The configuration the upstream's `auth` sub-configuration carried.
        config: Value,
    },
    /// A custom auth row the data plane executes through the sandbox.
    Custom {
        /// The persisted row the identifier resolved to.
        row: Plugin,
        /// The configuration the upstream's `auth` sub-configuration carried.
        config: Value,
    },
}

impl std::fmt::Debug for ComposedAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Noop => formatter.write_str("Noop"),
            Self::Builtin { config, .. } => formatter.debug_tuple("Builtin").field(config).finish(),
            Self::Custom { row, config } => formatter
                .debug_struct("Custom")
                .field("row", &row.id)
                .field("config", config)
                .finish(),
        }
    }
}

/// One composed binding: a resolved plugin at one position of one layer, with
/// the configuration it was bound with.
#[derive(Clone)]
pub enum ComposedStep {
    /// A built-in implementation the registry holds.
    Builtin {
        /// The canonical plugin identifier the binding was written with.
        plugin_ref: String,
        /// The chain position the binding was stored at.
        position: u32,
        /// The layer the binding came from: `true` for the upstream's own set.
        upstream_layer: bool,
        /// The configuration the binding carries.
        config: Value,
        /// The guard implementation, when the resolved plugin declares one.
        guard: Option<Arc<dyn crate::domain::plugin_contract::GuardPlugin>>,
        /// The transform implementation, when the resolved plugin declares one.
        transform: Option<Arc<dyn crate::domain::plugin_contract::TransformPlugin>>,
    },
    /// A persisted custom row the data plane executes through the sandbox.
    Custom {
        /// The canonical plugin identifier the binding was written with.
        plugin_ref: String,
        /// The chain position the binding was stored at.
        position: u32,
        /// The layer the binding came from: `true` for the upstream's own set.
        upstream_layer: bool,
        /// The configuration the binding carries.
        config: Value,
        /// The persisted row, whose declared phases the composition reads.
        row: Plugin,
    },
}

impl std::fmt::Debug for ComposedStep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (plugin_ref, position, upstream_layer, config) = match self {
            Self::Builtin {
                plugin_ref,
                position,
                upstream_layer,
                config,
                ..
            }
            | Self::Custom {
                plugin_ref,
                position,
                upstream_layer,
                config,
                ..
            } => (plugin_ref, position, upstream_layer, config),
        };
        formatter
            .debug_struct("ComposedStep")
            .field("plugin_ref", plugin_ref)
            .field("position", position)
            .field("upstream_layer", upstream_layer)
            .field("config", config)
            .finish()
    }
}

impl ComposedStep {
    /// The canonical plugin identifier the binding was written with.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Builtin { plugin_ref, .. } | Self::Custom { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The chain position the binding was stored at.
    #[must_use]
    pub const fn position(&self) -> u32 {
        match self {
            Self::Builtin { position, .. } | Self::Custom { position, .. } => *position,
        }
    }

    /// The layer the binding came from: `true` for the upstream's own set.
    #[must_use]
    pub const fn upstream_layer(&self) -> bool {
        match self {
            Self::Builtin { upstream_layer, .. } | Self::Custom { upstream_layer, .. } => {
                *upstream_layer
            }
        }
    }

    /// The phases the resolved plugin declares.
    ///
    /// A built-in implementation declares them through its `declares` answers;
    /// a persisted row declares them through the wire literals it was created
    /// with, which the composition reads off the row it resolved.
    #[must_use]
    pub fn declares(&self, phase: PluginPhase) -> bool {
        match self {
            Self::Builtin { guard, transform, .. } => {
                guard.as_ref().is_some_and(|guard| guard.declares(phase))
                    || transform.as_ref().is_some_and(|transform| transform.declares(phase))
            }
            Self::Custom { row, .. } => {
                let family = PluginFamily::from_type_literal(&row.plugin_type)
                    .unwrap_or(PluginFamily::Transform);
                row.phases
                    .iter()
                    .filter_map(|literal| {
                        crate::control_plane::plugin_def::phase_of(family, literal)
                    })
                    .any(|declared| declared == phase)
            }
        }
    }
}

/// One composed chain: the upstream's auth plugin and the five per-phase
/// sub-chains the binding sets compose to, each in composed order.
#[derive(Clone)]
pub struct ComposedChain {
    /// The auth step, resolved from the upstream's scalar identity columns.
    pub auth: ComposedAuth,
    /// Guards on the request.
    pub guard_request: Vec<ComposedStep>,
    /// Transforms on the request.
    pub transform_request: Vec<ComposedStep>,
    /// Guards on the response.
    pub guard_response: Vec<ComposedStep>,
    /// Transforms on the response.
    pub transform_response: Vec<ComposedStep>,
    /// Transforms on the error.
    pub transform_error: Vec<ComposedStep>,
}

impl std::fmt::Debug for ComposedChain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComposedChain")
            .field("auth", &self.auth)
            .field("guard_request", &self.guard_request)
            .field("transform_request", &self.transform_request)
            .field("guard_response", &self.guard_response)
            .field("transform_response", &self.transform_response)
            .field("transform_error", &self.transform_error)
            .finish()
    }
}

/// Composes the per-phase sub-chains of one request's plugin schedule.
///
/// The two layers are given in stored `position` order by the caller; the
/// composition orders them itself, upstream before route, so the caller cannot
/// hand it a layer order that contradicts DESIGN §3.2.
///
/// # Errors
///
/// Returns the 503 `PluginNotFound` gateway error for a composed binding that
/// resolves to no implementation, which is the answer the caller forwards; the
/// composition never silently drops a binding it was given.
#[allow(clippy::result_large_err)]
pub fn compose(
    store: &crate::store::OagwStore,
    tenant_id: Uuid,
    named: &crate::domain::plugin_contract::NamedPluginRegistry,
    registries: &PluginRegistries,
    auth: Option<(&str, Option<Uuid>, &Value)>,
    upstream: &[PluginBinding],
    route: &[PluginBinding],
) -> Result<ComposedChain, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-auth
    // The upstream's single auth plugin is resolved through the
    // reference-resolution routine from the scalar identity columns; an
    // upstream with none resolves to the no-op behaviour, and a route
    // contributes no auth phase at all.
    let composed_auth = match auth {
        None => ComposedAuth::Noop,
        Some((plugin_ref, plugin_uuid, config)) => {
            let resolved = binding::resolve(store, tenant_id, named, plugin_ref, plugin_uuid)
                .map_err(|failure| missing(&failure.reason()))?;
            match resolved {
                ResolvedPlugin::Named { .. } => ComposedAuth::Builtin {
                    plugin: registries.auth.resolve(plugin_ref).map_err(|_| {
                        missing("the bound auth plugin is no longer registered")
                    })?,
                    config: config.clone(),
                },
                ResolvedPlugin::Custom { id, .. } => ComposedAuth::Custom {
                    row: store
                        .get_plugin(tenant_id, id)
                        .map(|row| row.plugin)
                        .ok_or_else(|| missing("the bound auth plugin is no longer stored"))?,
                    config: config.clone(),
                },
            }
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-auth

    // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-order
    // The upstream layer's guard and transform bindings are ordered by
    // `position`, then the route layer's, and the two are concatenated in that
    // order.
    let mut ordered: Vec<(bool, &PluginBinding)> = Vec::with_capacity(upstream.len() + route.len());
    ordered.extend(upstream.iter().map(|item| (true, item)));
    ordered.extend(route.iter().map(|item| (false, item)));
    // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-order

    let mut composed: Vec<ComposedStep> = Vec::with_capacity(ordered.len());
    // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-loop
    for (upstream_layer, item) in ordered {
        // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-resolve
        // Each composed binding is resolved through the reference-resolution
        // routine, and the phases the implementation declares are recorded
        // beside it.
        let resolved = binding::resolve(
            store,
            tenant_id,
            named,
            &item.plugin_ref,
            item.plugin_uuid,
        )
        .map_err(|failure| missing(&failure.reason()))?;
        // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-resolve

        let step = match resolved {
            ResolvedPlugin::Named { family, .. } => {
                let (guard, transform) = implementations(registries, family, &item.plugin_ref)?;
                ComposedStep::Builtin {
                    plugin_ref: item.plugin_ref.clone(),
                    position: item.position,
                    upstream_layer,
                    config: item.config.clone(),
                    guard,
                    transform,
                }
            }
            ResolvedPlugin::Custom { id, .. } => ComposedStep::Custom {
                plugin_ref: item.plugin_ref.clone(),
                position: item.position,
                upstream_layer,
                config: item.config.clone(),
                row: store
                    .get_plugin(tenant_id, id)
                    .map(|row| row.plugin)
                    .ok_or_else(|| missing("the bound plugin is no longer stored"))?,
            },
        };
        composed.push(step);
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-loop

    // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-missing-if
    // A composed binding that resolved to no implementation is reported to the
    // caller rather than dropped: the bound plugin row was deleted after the
    // binding was written, or its identifier is no longer registered.
    if composed.len() != upstream.len() + route.len() {
        // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-missing
        return Err(missing("a bound plugin no longer resolves"));
        // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-missing
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-missing-if

    // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-phase-loop
    let phases = [
        (PluginPhase::GuardRequest, 0_u8),
        (PluginPhase::TransformRequest, 1),
        (PluginPhase::GuardResponse, 2),
        (PluginPhase::TransformResponse, 3),
        (PluginPhase::TransformError, 4),
    ];
    let mut subchains: [Vec<ComposedStep>; 5] = Default::default();
    for (phase, slot) in phases {
        // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-phase
        // The sub-chain of composed bindings whose implementation declares
        // that phase, preserving the composed order within it.
        subchains[usize::from(slot)] = composed
            .iter()
            .filter(|step| step.declares(phase))
            .cloned()
            .collect();
        // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-phase
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-phase-loop

    // @cpt-begin:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-return
    let [guard_request, transform_request, guard_response, transform_response, transform_error] =
        subchains;
    Ok(ComposedChain {
        auth: composed_auth,
        guard_request,
        transform_request,
        guard_response,
        transform_response,
        transform_error,
    })
    // @cpt-end:cpt-cf-oagw-algo-chain-compose:p1:inst-compose-return
}

/// The implementations one named plugin identifier answers: at most one of the
/// two, and always the one its family's own registry backs.
type NamedImpls = (
    Option<Arc<dyn crate::domain::plugin_contract::GuardPlugin>>,
    Option<Arc<dyn crate::domain::plugin_contract::TransformPlugin>>,
);

/// The implementations one named plugin identifier answers in its own family's
/// registry, and in no other.
#[allow(clippy::result_large_err)]
fn implementations(
    registries: &PluginRegistries,
    family: PluginFamily,
    plugin_ref: &str,
) -> Result<NamedImpls, DomainError> {
    match family {
        PluginFamily::Auth => Ok((None, None)),
        PluginFamily::Guard => Ok((
            Some(registries.guard.resolve(plugin_ref).map_err(|_| {
                missing("the bound guard plugin is no longer registered")
            })?),
            None,
        )),
        PluginFamily::Transform => Ok((
            None,
            Some(registries.transform.resolve(plugin_ref).map_err(|_| {
                missing("the bound transform plugin is no longer registered")
            })?),
        )),
    }
}

/// The 503 the unresolved reference is reported with, which the caller answers
/// through the foundation catalogue's `PluginNotFound` variant.
fn missing(reason: &str) -> DomainError {
    DomainError::gateway(ErrorKind::PluginNotFound, reason)
}
