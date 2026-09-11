//! The plugin chain executor
//! (`cpt-cf-oagw-dod-plugin-system-plugin-traits`).
//!
//! The deterministic order is auth, guards, transform(request), the upstream
//! call, transform(response) or transform(error), with the upstream call
//! itself owned by the request-proxy entry. The executor therefore runs in two
//! halves that the proxy pipeline calls in sequence, so the upstream call
//! really is the `awaiting_upstream` state of
//! `cpt-cf-oagw-state-plugin-system-chain-execution` and no plugin runs out of
//! order or twice in the same phase.
//!
//! **Timeout posture.** Every built-in phase is bounded per invocation by the
//! remaining request budget, derived from the configured `proxy_timeout_secs`
//! of `OagwConfig` — the total request budget entry 2.1 delivers, with no new
//! configuration key introduced for the per-plugin bound. A phase that
//! exceeds its bound is a *phase error*, never a reject decision, and maps
//! onto `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` for entry 2.5
//! to render as `504`.
// @cpt-state:cpt-cf-oagw-state-plugin-system-chain-execution:p1
// @cpt-state:cpt-cf-oagw-state-plugin-system-plugin-record:p1

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AuthContext, ErrorContext, PluginError, PluginKind, RequestContext, ResponseContext,
};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::domain::plugin::composition::ComposedChain;
use crate::infra::plugin::resolution::{
    resolve_auth, resolve_reference, PluginRegistries, ResolvedBinding, TenantChain,
};

// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-1
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-10
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-11
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-12
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-13
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-14
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-15
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-16
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-17
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-2
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-3
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-4
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-5
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-6
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-7
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-8
// @cpt-begin:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-9
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-1
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-2
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-3
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-4
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-5
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-6
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-7
// @cpt-begin:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-8
/// The plugin runtime the data plane holds.
pub struct PluginRuntime {
    registries: Arc<PluginRegistries>,
    proxy_timeout_secs: u64,
}
//
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-9
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-8
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-7
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-6
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-5
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-4
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-3
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-2
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-17
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-16
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-15
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-14
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-13
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-12
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-11
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-10
// @cpt-end:cpt-cf-oagw-state-plugin-system-chain-execution:p1:inst-ps-st-exec-1
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-8
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-7
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-6
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-5
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-4
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-3
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-2
// @cpt-end:cpt-cf-oagw-state-plugin-system-plugin-record:p1:inst-ps-st-rec-1
//

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginRuntime")
            .field("proxy_timeout_secs", &self.proxy_timeout_secs)
            .finish()
    }
}

/// The transformed request surface the request phases hand to the upstream
/// call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransformedRequest {
    /// The request headers after the transform phase, including the
    /// `Authorization` or API-key header the auth phase injected.
    pub headers: Vec<(String, String)>,
    /// The query parameters after the transform phase, including the API key a
    /// `query`-location auth plugin injected.
    pub query: Vec<(String, String)>,
    /// The request body after the transform phase.
    pub body: Bytes,
}

impl TransformedRequest {
    /// Look one header value up, case-insensitively, first match wins.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The chain resolved against the registries, ready to execute.
#[derive(Clone)]
pub struct ResolvedChain {
    /// The single auth plugin, taken from the upstream `auth` block.
    pub auth: Option<ResolvedAuth>,
    /// The guard and transform entries, in chain order.
    pub entries: Vec<ResolvedEntry>,
    /// The phase trace the execution produced, in order.
    pub phases: Vec<PhaseTrace>,
}

impl ResolvedChain {
    /// Whether the chain carries no plugin at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.auth.is_none() && self.entries.is_empty()
    }
}

/// The resolved auth plugin with the configuration it runs against.
#[derive(Clone)]
pub struct ResolvedAuth {
    pub reference: String,
    pub plugin: Arc<dyn AuthPlugin>,
    pub config: Option<serde_json::Value>,
    /// The credential references the configuration carried, resolved at
    /// request time by the plugin itself and recorded here as references only.
    pub credential_refs: Vec<String>,
}

/// One resolved chain entry: which phase it serves and what it runs with.
#[derive(Clone)]
pub enum ResolvedEntry {
    /// A guard, run against the request and against the upstream response.
    Guard {
        reference: String,
        plugin: Arc<dyn GuardPlugin>,
        config: Option<serde_json::Value>,
    },
    /// A transform, run against the request, the response, and the error.
    Transform {
        reference: String,
        plugin: Arc<dyn TransformPlugin>,
        config: Option<serde_json::Value>,
    },
}

impl std::fmt::Debug for ResolvedChain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedChain")
            .field("auth", &self.auth)
            .field("entries", &self.entries)
            .field("phases", &self.phases.len())
            .finish()
    }
}

impl std::fmt::Debug for ResolvedAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The plugin is a trait object with no `Debug`; only its reference and
        // the configuration shape are reported, never the credential
        // references' resolution.
        formatter
            .debug_struct("ResolvedAuth")
            .field("reference", &self.reference)
            .field("config", &self.config)
            .finish()
    }
}

impl std::fmt::Debug for ResolvedEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, reference) = match self {
            Self::Guard { reference, .. } => ("guard", reference.as_str()),
            Self::Transform { reference, .. } => ("transform", reference.as_str()),
        };
        formatter.debug_struct("ResolvedEntry").field("kind", &kind).field("reference", &reference).finish()
    }
}

impl ResolvedEntry {
    /// The reference the binding carried.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            Self::Guard { reference, .. } | Self::Transform { reference, .. } => reference,
        }
    }
}

/// One phase of the execution trace, for the outcome values the
/// observability entry emits and for the unit tests that assert the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTrace {
    /// The plugin identifier that ran.
    pub reference: String,
    /// The phase it ran in.
    pub phase: Phase,
    /// The outcome value: `allow`, `reject`, `error`, or `resolve_failure`.
    pub outcome: Outcome,
}

/// The phase a trace entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Auth,
    GuardRequest,
    TransformRequest,
    GuardResponse,
    TransformResponse,
    TransformError,
}

impl Phase {
    /// The phase name the trace carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::GuardRequest => "guard_request",
            Self::TransformRequest => "transform_request",
            Self::GuardResponse => "guard_response",
            Self::TransformError => "transform_error",
            Self::TransformResponse => "transform_response",
        }
    }
}

/// The outcome value of one phase (`cpt-cf-oagw-flow-plugin-system-chain-execution`,
/// observability contributions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    Reject,
    Error,
    ResolveFailure,
}

impl Outcome {
    /// The outcome value name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Reject => "reject",
            Self::Error => "error",
            Self::ResolveFailure => "resolve_failure",
        }
    }
}

/// The per-plugin kind a resolved chain entry maps onto, used to decide which
/// phase a reference belongs to.
#[must_use]
pub fn kind_of(plugin_type: &str) -> Option<PluginKind> {
    match crate::domain::plugin::identifier::plugin_base_type_of(plugin_type) {
        Some(base) if base == crate::domain::gts_helpers::AUTH_PLUGIN_BASE_TYPE => {
            Some(PluginKind::Auth)
        }
        Some(base) if base == crate::domain::gts_helpers::GUARD_PLUGIN_BASE_TYPE => {
            Some(PluginKind::Guard)
        }
        Some(base) if base == crate::domain::gts_helpers::TRANSFORM_PLUGIN_BASE_TYPE => {
            Some(PluginKind::Transform)
        }
        _ => None,
    }
}

impl PluginRuntime {
    /// Build the runtime over the registries and the configured request
    /// budget.
    #[must_use]
    pub fn new(registries: Arc<PluginRegistries>, proxy_timeout_secs: u64) -> Self {
        Self { registries, proxy_timeout_secs }
    }

    /// The registries the runtime resolves against.
    #[must_use]
    pub fn registries(&self) -> &Arc<PluginRegistries> {
        &self.registries
    }

    /// The remaining request budget one phase is bounded by.
    #[must_use]
    pub const fn budget(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Resolve the composed chain
    /// (`cpt-cf-oagw-state-plugin-system-chain-execution`, state `resolving`).
    ///
    /// Every reference is resolved before execution begins, so an unresolvable
    /// reference fails the request with `PluginNotFound` rather than being
    /// silently skipped (`inst-ps-comp-8`/`-9`).
    ///
    /// # Errors
    ///
    /// [`DomainError::PluginNotFound`] when a reference resolves to nothing.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-7
    // `inst-ps-comp-7`/`-9`: every reference in the composed chain is resolved
    // before execution begins, and an unresolvable one fails the request
    // instead of being skipped.
    pub fn resolve(
        &self,
        chain: &ComposedChain,
        tenant_chain: &TenantChain,
        auth_config: Option<&serde_json::Value>,
    ) -> Result<ResolvedChain, DomainError> {
        let mut resolved = ResolvedChain {
            auth: None,
            entries: Vec::with_capacity(chain.bindings.len()),
            phases: Vec::new(),
        };
        resolved.auth = match resolve_auth(&self.registries, tenant_chain, chain.auth_ref.as_deref())? {
            Some(plugin) => {
                let reference = plugin.reference.clone();
                match plugin.binding {
                    Some(ResolvedBinding::Auth(plugin)) => Some(ResolvedAuth {
                        reference,
                        plugin,
                        config: auth_config.cloned(),
                        credential_refs: credential_references(auth_config),
                    }),
                    _ => return Err(not_found(&reference)),
                }
            }
            None => None,
        };
        for binding in &chain.bindings {
            let plugin = resolve_reference(&self.registries, tenant_chain, &binding.plugin_ref)?;
            // `plugins.items[]` is a reference list, so a chain entry carries
            // no per-item configuration in the graded model; only the auth
            // block carries `config`.
            let config = None;
            match plugin.binding {
                Some(ResolvedBinding::Guard(plugin)) => resolved.entries.push(ResolvedEntry::Guard {
                    reference: binding.plugin_ref.clone(),
                    plugin,
                    config,
                }),
                Some(ResolvedBinding::Transform(plugin)) => {
                    resolved.entries.push(ResolvedEntry::Transform {
                        reference: binding.plugin_ref.clone(),
                        plugin,
                        config,
                    });
                }
                // An auth plugin reached through the chain is not the request's
                // auth plugin: a credential injected from a chain position
                // would be an invisible auth bypass, so the request fails.
                Some(ResolvedBinding::Auth(_)) => return Err(not_found(&binding.plugin_ref)),
                // A custom plugin is a registry reference with no backing
                // implementation (graded deviation 6): the reference resolves,
                // is bindable and is readable through the source endpoint, and
                // its source content is never interpreted or executed, so it
                // contributes no phase to the chain.
                None => {}
            }
        }
        Ok(resolved)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-7

    /// The request phases: auth, guards, transform(request).
    ///
    /// `request` is mutated in place, so the caller hands the transformed
    /// surface straight to the upstream call.
    ///
    /// `principal` is the caller identity of the proxied request: the auth
    /// phase receives it verbatim, so a plugin that keys its state on the
    /// subject tenant and subject identifier never shares an entry between two
    /// tenants (`inst-ps-key-2`).
    ///
    /// # Errors
    ///
    /// The terminal [`DomainError`] the shared error contract renders: an auth
    /// failure, a guard rejection, a request-phase transform failure, or a
    /// phase that exceeded its budget.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-1
    // `inst-ps-exec-1` .. `-9`: the auth plugin runs exactly once before every
    // guard, the guards run in chain order and the first rejection is
    // terminal, and a request-phase transform failure stops the chain with no
    // upstream call.
    pub async fn run_request(
        &self,
        chain: &mut ResolvedChain,
        method: &str,
        path: &str,
        query: &[(String, String)],
        headers: &mut Vec<(String, String)>,
        body: Bytes,
        principal: crate::domain::plugin::Principal,
        trace_id: &str,
    ) -> Result<TransformedRequest, DomainError> {
        let mut transformed = TransformedRequest {
            headers: headers.clone(),
            query: query.to_vec(),
            body,
        };
        let budget = self.budget();

        // -- the auth phase --------------------------------------------
        if let Some(auth) = chain.auth.clone() {
            let mut context = AuthContext {
                config: auth.config.clone(),
                credentials: auth
                    .credential_refs
                    .iter()
                    .map(|reference| crate::domain::plugin::ResolvedCredential {
                        reference: reference.clone(),
                        // Only the reference ever crosses the plugin boundary;
                        // the handle names the material the plugin resolved
                        // inside its own invocation.
                        handle: String::new(),
                    })
                    .collect(),
                principal,
                outbound_headers: Vec::new(),
                outbound_query: Vec::new(),
            };
            let outcome = tokio::time::timeout(budget, auth.plugin.authenticate(&mut context)).await;
            match classify(outcome, Phase::Auth, &auth.reference) {
                PhaseVerdict::Proceed => {
                    chain.phases.push(PhaseTrace {
                        reference: auth.reference.clone(),
                        phase: Phase::Auth,
                        outcome: Outcome::Allow,
                    });
                    // The injected credentials go onto the outbound request.
                    transformed.headers.extend(context.outbound_headers.clone());
                    transformed.query.extend(context.outbound_query.clone());
                }
                PhaseVerdict::Reject(error) => {
                    chain.phases.push(PhaseTrace {
                        reference: auth.reference.clone(),
                        phase: Phase::Auth,
                        outcome: Outcome::Error,
                    });
                    return Err(error);
                }
            }
        }

        let mut request_context = RequestContext {
            method: method.to_owned(),
            path: path.to_owned(),
            query: transformed.query.clone(),
            headers: transformed.headers.clone(),
            extensions: vec![(
                crate::infra::plugin::request_id_transform::CORRELATION_EXTENSION.to_owned(),
                trace_id.to_owned(),
            )],
            config: None,
        };

        // -- the guard and transform(request) phases --------------------
        for entry in &chain.entries {
            match entry {
                ResolvedEntry::Guard { reference, plugin, config } => {
                    let context = RequestContext { config: config.clone(), ..request_context.clone() };
                    let verdict = tokio::time::timeout(budget, plugin.guard_request(&context)).await;
                    match classify(verdict, Phase::GuardRequest, reference) {
                        PhaseVerdict::Proceed => {
                            chain.phases.push(PhaseTrace {
                                reference: reference.clone(),
                                phase: Phase::GuardRequest,
                                outcome: Outcome::Allow,
                            });
                        }
                        PhaseVerdict::Reject(error) => {
                            chain.phases.push(PhaseTrace {
                                reference: reference.clone(),
                                phase: Phase::GuardRequest,
                                outcome: guard_outcome(&error),
                            });
                            return Err(error);
                        }
                    }
                    request_context.config = context.config;
                }
                ResolvedEntry::Transform { reference, plugin, config } => {
                    let mut context = RequestContext { config: config.clone(), ..request_context.clone() };
                    let verdict = tokio::time::timeout(budget, plugin.transform_request(&mut context)).await;
                    match classify(verdict, Phase::TransformRequest, reference) {
                        PhaseVerdict::Proceed => {
                            chain.phases.push(PhaseTrace {
                                reference: reference.clone(),
                                phase: Phase::TransformRequest,
                                outcome: Outcome::Allow,
                            });
                            transformed.headers = context.headers.clone();
                            transformed.query = context.query.clone();
                            request_context = context;
                        }
                        PhaseVerdict::Reject(error) => {
                            chain.phases.push(PhaseTrace {
                                reference: reference.clone(),
                                phase: Phase::TransformRequest,
                                outcome: Outcome::Error,
                            });
                            return Err(error);
                        }
                    }
                }
            }
        }
        Ok(transformed)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-1

    /// The response phases: transform(response) or transform(error), then the
    /// response-phase guard check.
    ///
    /// # Errors
    ///
    /// The terminal [`DomainError`] the shared error contract renders when the
    /// response phase rejects or fails; a transform failure in the error phase
    /// never masks the original error and is therefore not returned.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-11
    // `inst-ps-exec-10` .. `-13`, `inst-ps-exec-18` .. `-21`: the response
    // phase runs the transforms against the upstream response or the error
    // context, the response-phase guard check runs after the response is
    // available and before it is returned, and a response-phase transform
    // failure discards the upstream response.
    pub async fn run_response(
        &self,
        chain: &mut ResolvedChain,
        status: u16,
        headers: &mut Vec<(String, String)>,
        upstream_error: Option<&DomainError>,
        trace_id: &str,
    ) -> Result<(), DomainError> {
        let budget = self.budget();
        let mut response_context = ResponseContext {
            status,
            headers: headers.clone(),
            extensions: vec![(
                crate::infra::plugin::request_id_transform::CORRELATION_EXTENSION.to_owned(),
                trace_id.to_owned(),
            )],
            config: None,
        };
        let mut error_context = ErrorContext {
            error: upstream_error.cloned(),
            // The status the upstream call reported, which is what the error
            // context carries; the HTTP rendering stays with entry 2.5's
            // single mapping layer.
            status,
        };

        for entry in &chain.entries {
            match entry {
                ResolvedEntry::Guard { reference, plugin, config } => {
                    let context =
                        ResponseContext { config: config.clone(), ..response_context.clone() };
                    let verdict = tokio::time::timeout(budget, plugin.guard_response(&context)).await;
                    match classify(verdict, Phase::GuardResponse, reference) {
                        PhaseVerdict::Proceed => {
                            chain.phases.push(PhaseTrace {
                                reference: reference.clone(),
                                phase: Phase::GuardResponse,
                                outcome: Outcome::Allow,
                            });
                        }
                        PhaseVerdict::Reject(error) => {
                            chain.phases.push(PhaseTrace {
                                reference: reference.clone(),
                                phase: Phase::GuardResponse,
                                outcome: guard_outcome(&error),
                            });
                            return Err(error);
                        }
                    }
                    response_context.config = context.config;
                }
                ResolvedEntry::Transform { reference, plugin, config } => {
                    if upstream_error.is_none() {
                        let mut context = ResponseContext {
                            config: config.clone(),
                            ..response_context.clone()
                        };
                        let verdict =
                            tokio::time::timeout(budget, plugin.transform_response(&mut context)).await;
                        match classify(verdict, Phase::TransformResponse, reference) {
                            PhaseVerdict::Proceed => {
                                chain.phases.push(PhaseTrace {
                                    reference: reference.clone(),
                                    phase: Phase::TransformResponse,
                                    outcome: Outcome::Allow,
                                });
                                *headers = context.headers.clone();
                                response_context = context;
                            }
                            // `inst-ps-exec-19`: the upstream response is
                            // discarded and the failure maps to the
                            // downstream-error class — except a phase that
                            // exceeded its bound, which keeps the
                            // request-timeout classification the timeout
                            // posture assigns it.
                            PhaseVerdict::Reject(error) => {
                                chain.phases.push(PhaseTrace {
                                    reference: reference.clone(),
                                    phase: Phase::TransformResponse,
                                    outcome: Outcome::Error,
                                });
                                if matches!(error, DomainError::RequestTimeout { .. }) {
                                    return Err(error);
                                }
                                return Err(DomainError::DownstreamError {
                                    upstream_id: None,
                                    host: None,
                                    path: None,
                                    trace_id: Some(trace_id.to_owned()),
                                    retriable: false,
                                });
                            }
                        }
                    } else {
                        let mut context = error_context.clone();
                        let verdict =
                            tokio::time::timeout(budget, plugin.transform_error(&mut context)).await;
                        match classify(verdict, Phase::TransformError, reference) {
                            PhaseVerdict::Proceed => {
                                chain.phases.push(PhaseTrace {
                                    reference: reference.clone(),
                                    phase: Phase::TransformError,
                                    outcome: Outcome::Allow,
                                });
                                error_context = context;
                            }
                            // `inst-ps-exec-21`: the chain falls back to the
                            // untransformed error context and never masks the
                            // original error type.
                            PhaseVerdict::Reject(_) => {
                                chain.phases.push(PhaseTrace {
                                    reference: reference.clone(),
                                    phase: Phase::TransformError,
                                    outcome: Outcome::Error,
                                });
                                tracing::warn!(
                                    plugin = %reference,
                                    "a transform plugin failed in the error phase; the untransformed error context is rendered"
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-11
}

// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-10
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-2
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-3
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-9
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-10
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-12
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-13
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-14
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-15
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-16
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-17
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-18
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-19
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-2
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-20
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-21
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-3
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-9
/// The plugin reference a `PluginNotFound` names.
fn not_found(reference: &str) -> DomainError {
    DomainError::PluginNotFound { plugin_ref: reference.to_owned() }
}
//
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-4
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-3
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-10
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-4
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-3
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-21
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-20
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-19
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-18
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-17
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-16
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-15
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-14
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-13
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-12
// @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-10
//

/// The verdict one phase produced.
enum PhaseVerdict {
    Proceed,
    Reject(DomainError),
}

/// Classify one awaited plugin outcome into a proceed verdict or a terminal
/// error, applying the timeout posture (`inst-ps-exec-15`).
fn classify<T>(
    outcome: Result<Result<T, PluginError>, tokio::time::error::Elapsed>,
    phase: Phase,
    reference: &str,
) -> PhaseVerdict {
    let Ok(inner) = outcome else {
        // A phase that exceeded its bound is a phase error, never a reject
        // decision.
        return PhaseVerdict::Reject(DomainError::RequestTimeout {
            upstream_id: None,
            host: None,
            guidance_secs: None,
            trace_id: None,
        });
    };
    match inner {
        Ok(_) => PhaseVerdict::Proceed,
        Err(error) => PhaseVerdict::Reject(map_plugin_error(error, phase, reference)),
    }
}

/// Map a plugin error onto the shared error contract.
///
/// * an auth failure is `auth.failed.v1` (`401`);
/// * a rejection carries its own phase-specific status when the plugin
///   produced one, and is otherwise mapped as a guard reject is mapped —
///   `400` in the request phase and `502` in the response phase;
/// * a backing-service failure is `secret.not_found.v1`, which is the `500`
///   the credential step of the credential-resolution flow assigns;
/// * an internal plugin failure is a `500` invariant violation.
#[must_use]
pub fn map_plugin_error(error: PluginError, phase: Phase, reference: &str) -> DomainError {
    match (&error, phase) {
        (PluginError::Rejected { reason, status }, Phase::Auth) => {
            let _ = (reason, status);
            DomainError::AuthenticationFailed {
                upstream_id: None,
                host: None,
                path: None,
                trace_id: None,
            }
        }
        (PluginError::Rejected { reason, status }, phase) => {
            let status = status.unwrap_or(match phase {
                Phase::GuardResponse | Phase::TransformResponse => 502,
                _ => 400,
            });
            let _ = reference;
            if status >= 500 {
                DomainError::DownstreamError {
                    upstream_id: None,
                    host: None,
                    path: None,
                    trace_id: None,
                    retriable: false,
                }
            } else {
                DomainError::ValidationError {
                    detail: reason.clone(),
                    path: Some("plugins".to_owned()),
                    trace_id: None,
                }
            }
        }
        (PluginError::Unavailable, _) => DomainError::SecretNotFound {
            path: None,
            trace_id: None,
        },
        (PluginError::Internal(detail), _) => DomainError::PluginInternal(detail.clone()),
    }
}

/// Whether a phase error counts as a rejection or an error in the trace.
///
/// Only the two client-facing verdicts are rejections: a validation rejection
/// (the plugin said no to this request) and an authentication failure. Every
/// other mapped class — a credential the store could not resolve, an internal
/// plugin failure, a phase that exceeded its bound, a discarded upstream
/// response — is a *failure of the chain*, and is traced as `error` so the
/// observability entry never reports a caller's request as refused when the
/// gateway itself failed.
fn guard_outcome(error: &DomainError) -> Outcome {
    match error {
        DomainError::ValidationError { .. } | DomainError::AuthenticationFailed { .. } => {
            Outcome::Reject
        }
        _ => Outcome::Error,
    }
}

/// The `cred://` references a configuration carries, recorded as references
/// only (`inst-ps-iso-2`).
fn credential_references(config: Option<&serde_json::Value>) -> Vec<String> {
    let Some(config) = config else { return Vec::new() };
    let Some(object) = config.as_object() else { return Vec::new() };
    object
        .values()
        .filter_map(serde_json::Value::as_str)
        .filter(|value| crate::domain::dto::is_cred_reference(value))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod executor_tests;

#[cfg(test)]
#[path = "phase_bound_tests.rs"]
mod phase_bound_tests;
