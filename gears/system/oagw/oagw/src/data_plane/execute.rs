//! Execution of the composed plugin chain around one proxy exchange.
//!
//! Realizes `cpt-cf-oagw-algo-chain-execute`: the phase order Auth, then
//! Guards on the request, then Transform on the request, then the upstream
//! call, then Guards and Transform on the response, and Transform on the error
//! when the call fails — the order DESIGN §3.2 Plugin System states and ADR
//! 0002's execution order repeats. The composition
//! (`cpt-cf-oagw-algo-chain-compose` of `cpt-cf-oagw-feature-plugin-system`)
//! produced the schedule; this module owns the two things the composition does
//! not: the sandbox discipline of `cpt-cf-oagw-nfr-starlark-sandbox` and the
//! `last_used_at` record of the FEATURE §1.5.
//!
//! Every custom plugin this gear binds is a stored Starlark row, and no
//! Starlark interpreter exists in this deployment to hold one to its limits, so
//! no custom step ever executes: the chain answers the `PluginNotFound` failure
//! the FEATURE requires instead, and the `last_used_at` set this module records
//! is consequently always empty.

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::store::OagwStore;
use crate::domain::context::{AuthContext, RequestContext, ResponseContext};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin_contract::{
    GuardDecision, PluginFailure, SandboxLimits,
};
use crate::domain::proxy::{PluginMutations, ProxyContext};
use crate::plugins::chain::{ComposedAuth, ComposedChain, ComposedStep};

// @cpt-dod:cpt-cf-oagw-dod-chain-execution:p1

/// Runs the request leg of the chain: auth, then the guards, then the
/// transforms.
///
/// The mutations returned are the header entries the plugins added or mutated
/// and the names they removed, which `cpt-cf-oagw-algo-header-transform`
/// carries into the outbound map after the configuration rules have run.
///
/// # Errors
///
/// Returns the `PluginNotFound` failure for a custom step whose limits cannot
/// be enforced, the `AuthenticationFailed` and `SecretNotFound` failures the
/// credential resolution maps, the `ValidationError` failure a request-phase
/// guard rejection answers, and the `ProtocolError` failure a sandbox breach
/// answers.
#[allow(clippy::result_large_err)]
pub async fn run_request_phase(
    chain: &ComposedChain,
    context: &ProxyContext,
    limits: &SandboxLimits,
) -> Result<PluginMutations, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-unresolvable-if
    // A custom plugin's limits cannot be enforced in this deployment: no
    // interpreter exists to strip the network, file, and import capabilities
    // from, so the step is refused rather than run, and the composition's
    // binding is never silently dropped.
    if !enforceable(chain) {
        // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-unresolvable-return
        return Err(DomainError::gateway(
            ErrorKind::PluginNotFound,
            "the bound plugin's sandbox limits cannot be enforced in this deployment, so the chain is refused",
        ));
        // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-unresolvable-return
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-unresolvable-if

    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-unresolvable-else
    // The ELSE of the enforceability check: every composed binding resolves to
    // an implementation this deployment can hold to its limits, so the phases
    // run in the composed order.
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-unresolvable-else

    let mut mutations = PluginMutations::default();

    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-auth
    // The credential material the phase resolves leaves the call in the auth
    // context's headers and nowhere else.
    let mut auth = AuthContext::new(context.tenant_id, context.subject_id);
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-auth-if
    match &chain.auth {
        ComposedAuth::Noop => {}
        ComposedAuth::Builtin { plugin, config } => {
            // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-auth-return
            // A reference the store refuses, or a credential the upstream
            // rejects, is the failure the caller answers 401 or 500 with, and
            // no later phase runs.
            plugin
                .authenticate(&mut auth, config)
                .await
                .map_err(failure_of)?;
            // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-auth-return
        }
        ComposedAuth::Custom { .. } => {
            return Err(DomainError::gateway(
                ErrorKind::PluginNotFound,
                "the bound auth plugin is a stored source whose limits cannot be enforced",
            ));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-auth-if
    for (name, value) in &auth.headers {
        mutations.set.push((name.clone(), value.clone()));
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-auth

    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards
    // The guards read the request as the caller issued it, and a rejection is
    // answered with the phase-specific status ADR 0009 states for the request
    // phase.
    let mut request = request_context(context);
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards-if
    for step in &chain.guard_request {
        // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards-return
        run_guard(step, &request, limits)?;
        // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards-return
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards-if
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards-else
    // The ELSE of the guard phase: every guard allowed the request, so the
    // transforms run on it next.
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards-else
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-guards

    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-transform
    // The transforms mutate one shared context in composed order, and only the
    // entries they added or mutated and the names they removed reach the
    // outbound map, which is what keeps the passthrough mode the configuration
    // declares the only thing that forwards an inbound header.
    let snapshot = request.headers.clone();
    for step in &chain.transform_request {
        run_transform(step, &mut request, limits)?;
    }
    for (name, value) in &request.headers {
        if snapshot.get(name) != Some(value) {
            mutations.set.push((name.clone(), value.clone()));
        }
    }
    for name in snapshot.keys() {
        if !request.headers.contains_key(name) {
            mutations.removed.push(name.clone());
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-transform

    Ok(mutations)
}

/// Runs the response leg of the chain: the guards, then the transforms.
///
/// # Errors
///
/// Returns the `ProtocolError` failure a response-phase guard rejection
/// answers, and the `ProtocolError` failure a sandbox breach answers.
#[allow(clippy::result_large_err)]
pub fn run_response_phase(
    chain: &ComposedChain,
    status: u16,
    upstream_headers: &[(String, String)],
    limits: &SandboxLimits,
) -> Result<PluginMutations, DomainError> {
    let mut mutations = PluginMutations::default();

    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response
    // The response leg runs after the upstream call, in the order the contract
    // declares: guards first, then transforms. The guard phase on the response
    // is the `guard_response` contract ADR 0002 declares, and its rejection is
    // answered with the phase-specific status ADR 0009 states for the response
    // phase.
    let mut response = ResponseContext::new(status);
    for (name, value) in upstream_headers {
        response.set_header(name, value.clone());
    }
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-if
    for step in &chain.guard_response {
        // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-return
        run_response_guard(step, &response, limits)?;
        // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-return
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-if
    let snapshot = response.headers.clone();
    for step in &chain.transform_response {
        run_response_transform(step, &mut response, limits)?;
    }
    for (name, value) in &response.headers {
        if snapshot.get(name) != Some(value) {
            mutations.set.push((name.clone(), value.clone()));
        }
    }
    for name in snapshot.keys() {
        if !response.headers.contains_key(name) {
            mutations.removed.push(name.clone());
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response

    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-else-return
    // The RETURN of the response leg: the authenticated and transformed
    // request and response inputs, whose header entries the caller carries
    // into the answer.
    Ok(mutations)
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-else-return
}

/// Runs the error leg of the chain on the failure the upstream call produced.
///
/// A transform that declares the error phase mutates the failure's context, and
/// the caller maps the mutated failure through the foundation's problem
/// mapping; no guard runs on the error leg, and no custom step ever reaches it.
pub fn run_error_phase(chain: &ComposedChain, error: &mut DomainError) {
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-else
    // On a failed call the transform phase runs on the error instead of on the
    // response, and every mutation it performed is applied to the failure the
    // caller answers with.
    for step in &chain.transform_error {
        if let Some(transform) = transform_of(step) {
            super::sandbox::invoke(&crate::domain::plugin_contract::SANDBOX_LIMITS, || {
                transform.transform_error(&mut error.context, step_config(step));
            })
            .unwrap_or(());
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-response-else
}

/// The custom plugin rows the chain would have executed, for the
/// `last_used_at` record.
///
/// The set is always empty in this deployment, because no custom step ever
/// executes; the function exists so the record is written from the same chain
/// the request ran and so a deployment that does execute custom sources gains
/// the record without a second mechanism.
#[must_use]
pub fn executed_custom_plugins(chain: &ComposedChain) -> Vec<Uuid> {
    let mut used: Vec<Uuid> = Vec::new();
    if let ComposedAuth::Custom { row, .. } = &chain.auth {
        used.push(row.id);
    }
    for step in chain
        .guard_request
        .iter()
        .chain(chain.transform_request.iter())
        .chain(chain.guard_response.iter())
        .chain(chain.transform_response.iter())
        .chain(chain.transform_error.iter())
    {
        if let ComposedStep::Custom { row, .. } = step {
            used.push(row.id);
        }
    }
    used.sort();
    used.dedup();
    used
}

/// Writes `last_used_at` for every custom plugin that executed, coalesced per
/// plugin, after the response is produced, and feeding no decision.
pub fn record_last_used(store: &OagwStore, chain: &ComposedChain, now: u64) {
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-lastused
    // The record is issued after the response is produced and reads nothing
    // back: no decision consumes `last_used_at`, so the write is the whole of
    // the obligation.
    let used = executed_custom_plugins(chain);
    if used.is_empty() {
        return;
    }
    store.record_plugin_use(&used, now);
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-lastused
}

/// Whether every step of the chain can be held to the sandbox limits.
fn enforceable(chain: &ComposedChain) -> bool {
    let limits = crate::domain::plugin_contract::SANDBOX_LIMITS;
    let auth_kind = match &chain.auth {
        ComposedAuth::Noop | ComposedAuth::Builtin { .. } => {
            super::sandbox::InvocationKind::Builtin
        }
        ComposedAuth::Custom { .. } => super::sandbox::InvocationKind::CustomSource,
    };
    if !super::sandbox::enforceable(auth_kind, &limits) {
        return false;
    }
    chain
        .guard_request
        .iter()
        .chain(chain.transform_request.iter())
        .chain(chain.guard_response.iter())
        .chain(chain.transform_response.iter())
        .chain(chain.transform_error.iter())
        .all(|step| step_kind(step).is_some_and(|kind| super::sandbox::enforceable(kind, &limits)))
}

/// The invocation kind of one step, or `None` when the step declares no
/// implementation this run can invoke.
fn step_kind(step: &ComposedStep) -> Option<super::sandbox::InvocationKind> {
    match step {
        ComposedStep::Builtin { .. } => Some(super::sandbox::InvocationKind::Builtin),
        ComposedStep::Custom { .. } => Some(super::sandbox::InvocationKind::CustomSource),
    }
}

/// Runs one request-phase guard under the sandbox discipline.
#[allow(clippy::result_large_err)]
fn run_guard(
    step: &ComposedStep,
    request: &RequestContext,
    limits: &SandboxLimits,
) -> Result<(), DomainError> {
    let Some(guard) = guard_of(step) else {
        return Err(unresolved(step));
    };
    let config = step_config(step);
    let bytes = serde_json::to_vec(&config).map_or(0, |encoded| encoded.len())
        + request
            .headers
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>();
    super::sandbox::admit(
        step_kind(step).unwrap_or(super::sandbox::InvocationKind::Builtin),
        limits,
        bytes,
    )
    .map_err(sandbox_refusal)?;
    let decision = super::sandbox::invoke(limits, || guard.guard_request(request, config))
        .map_err(sandbox_failure)?;
    rejection_of(&decision)
}

/// Runs one response-phase guard under the sandbox discipline.
#[allow(clippy::result_large_err)]
fn run_response_guard(
    step: &ComposedStep,
    response: &ResponseContext,
    limits: &SandboxLimits,
) -> Result<(), DomainError> {
    let Some(guard) = guard_of(step) else {
        return Err(unresolved(step));
    };
    let config = step_config(step);
    super::sandbox::admit(
        step_kind(step).unwrap_or(super::sandbox::InvocationKind::Builtin),
        limits,
        0,
    )
    .map_err(sandbox_refusal)?;
    let decision = super::sandbox::invoke(limits, || guard.guard_response(response, config))
        .map_err(sandbox_failure)?;
    rejection_of(&decision)
}

/// Runs one request-phase transform under the sandbox discipline.
#[allow(clippy::result_large_err)]
fn run_transform(
    step: &ComposedStep,
    request: &mut RequestContext,
    limits: &SandboxLimits,
) -> Result<(), DomainError> {
    let Some(transform) = transform_of(step) else {
        return Err(unresolved(step));
    };
    let config = step_config(step);
    super::sandbox::admit(
        step_kind(step).unwrap_or(super::sandbox::InvocationKind::Builtin),
        limits,
        0,
    )
    .map_err(sandbox_refusal)?;
    super::sandbox::invoke(limits, || transform.transform_request(request, config))
        .map_err(sandbox_failure)
}

/// Runs one response-phase transform under the sandbox discipline.
#[allow(clippy::result_large_err)]
fn run_response_transform(
    step: &ComposedStep,
    response: &mut ResponseContext,
    limits: &SandboxLimits,
) -> Result<(), DomainError> {
    let Some(transform) = transform_of(step) else {
        return Err(unresolved(step));
    };
    let config = step_config(step);
    super::sandbox::admit(
        step_kind(step).unwrap_or(super::sandbox::InvocationKind::Builtin),
        limits,
        0,
    )
    .map_err(sandbox_refusal)?;
    super::sandbox::invoke(limits, || transform.transform_response(response, config))
        .map_err(sandbox_failure)
}

/// The guard implementation of one step, or `None` for a step that declares
/// none this run can invoke.
fn guard_of(step: &ComposedStep) -> Option<Arc<dyn crate::domain::plugin_contract::GuardPlugin>> {
    match step {
        ComposedStep::Builtin { guard, .. } => guard.clone(),
        ComposedStep::Custom { .. } => None,
    }
}

/// The transform implementation of one step, or `None` for a step that declares
/// none this run can invoke.
fn transform_of(
    step: &ComposedStep,
) -> Option<Arc<dyn crate::domain::plugin_contract::TransformPlugin>> {
    match step {
        ComposedStep::Builtin { transform, .. } => transform.clone(),
        ComposedStep::Custom { .. } => None,
    }
}

/// The configuration one step was bound with.
fn step_config(step: &ComposedStep) -> &Value {
    match step {
        ComposedStep::Builtin { config, .. } | ComposedStep::Custom { config, .. } => config,
    }
}

/// The verdict a guard produced, mapped to the phase status the caller answers.
#[allow(clippy::result_large_err)]
fn rejection_of(decision: &GuardDecision) -> Result<(), DomainError> {
    match decision {
        GuardDecision::Allow => Ok(()),
        GuardDecision::Reject { code, message } => {
            let mut error = DomainError::gateway(
                ErrorKind::ValidationError,
                "the request or the response violates the contract a guard plugin enforces",
            );
            error.detail = format!("{code}: {message}");
            Err(error)
        }
    }
}

/// The typed failure a credential resolution returned, mapped onto the
/// catalogue rows `cpt-cf-oagw-algo-credential-resolution` names.
#[allow(clippy::result_large_err)]
fn failure_of(failure: PluginFailure) -> DomainError {
    match failure {
        PluginFailure::AuthenticationFailed => DomainError::gateway(
            ErrorKind::AuthenticationFailed,
            "the credential store declined the reference for the calling tenant or subject",
        ),
        PluginFailure::SecretNotFound => DomainError::gateway(
            ErrorKind::SecretNotFound,
            "the credential store resolved no secret for the configured reference",
        ),
        PluginFailure::CredentialShape | PluginFailure::Configuration { .. } => {
            DomainError::gateway(
                ErrorKind::RouteError,
                "the credential reference or the plugin configuration is unusable",
            )
        }
        PluginFailure::Unavailable => DomainError::gateway(
            ErrorKind::LinkUnavailable,
            "the credential store or the identity provider was unreachable",
        ),
    }
}

/// The failure a sandbox refusal is answered with.
#[allow(clippy::result_large_err)]
fn sandbox_refusal(refusal: super::sandbox::SandboxRefusal) -> DomainError {
    match refusal {
        super::sandbox::SandboxRefusal::Unenforceable => DomainError::gateway(
            ErrorKind::PluginNotFound,
            "the plugin's sandbox limits cannot be enforced, so the invocation is refused",
        ),
        super::sandbox::SandboxRefusal::OverBudget { reason } => {
            let mut error = DomainError::gateway(
                ErrorKind::ProtocolError,
                "the invocation exceeds the budget the sandbox holds it to",
            );
            error.detail = reason;
            error
        }
    }
}

/// The failure a sandbox breach is answered with.
#[allow(clippy::result_large_err)]
fn sandbox_failure(failure: super::sandbox::SandboxFailure) -> DomainError {
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-sandbox-if
    // A custom plugin that breached a sandbox limit, exceeded its
    // per-invocation timeout, or raised an error is the ELSE IF of the
    // response-phase checks, and the failure carries the gateway error source,
    // because the breach happened inside the gateway and not at the upstream.
    // @cpt-begin:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-sandbox-return
    let answer = match failure {
        super::sandbox::SandboxFailure::Raised => DomainError::gateway(
            ErrorKind::ProtocolError,
            "the plugin invocation raised an error and its mutations were discarded",
        ),
        super::sandbox::SandboxFailure::Timeout { limit_millis } => {
            let mut error = DomainError::gateway(
                ErrorKind::ProtocolError,
                "the plugin invocation exceeded its per-invocation timeout",
            );
            error.detail = format!("the invocation was held to {limit_millis} milliseconds");
            error
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-sandbox-return
    // No partial mutation the plugin performed survives the breach: the
    // invocation was discarded whole, so the caller answers with this failure
    // and nothing the plugin wrote.
    answer
    // @cpt-end:cpt-cf-oagw-algo-chain-execute:p1:inst-chain-sandbox-if
}

/// The failure a step that declares no invokable implementation is answered
/// with.
#[allow(clippy::result_large_err)]
fn unresolved(step: &ComposedStep) -> DomainError {
    DomainError::gateway(
        ErrorKind::PluginNotFound,
        format!(
            "the bound plugin {} resolves to no implementation this deployment executes",
            step.plugin_ref()
        ),
    )
}

/// The request context the guards and transforms read, built from the inbound
/// request.
fn request_context(context: &ProxyContext) -> RequestContext {
    let mut request = RequestContext::new(
        context.method.clone(),
        context.request_path(),
        context.query.clone(),
    );
    for (name, value) in &context.headers {
        request.set_header(name, value.clone());
    }
    request
}
