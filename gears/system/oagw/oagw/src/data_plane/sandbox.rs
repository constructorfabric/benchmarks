//! The Starlark sandbox discipline one plugin invocation is held to.
//!
//! Realizes `cpt-cf-oagw-algo-starlark-sandbox`: the capability check, the two
//! per-invocation ceilings, and the try/catch the invocation runs under. It is
//! the enforcement point `cpt-cf-oagw-nfr-starlark-sandbox` names, whose
//! threshold is "Zero sandbox escapes; plugin execution timeout ≤ 100ms; memory
//! ≤ 10MB per invocation", and which the plugin contract's
//! [`crate::domain::plugin_contract::SANDBOX_LIMITS`] exposes without
//! enforcing.
//!
//! The enforcement is absolute in this deployment: the gear carries no Starlark
//! interpreter and none may be added, so an invocation of a stored custom
//! source is a limit that cannot be enforced — no network capability can be
//! removed from an interpreter that does not exist, and no ceiling can be
//! measured around a run that never happens — and the invocation is refused
//! before it is attempted. A built-in implementation is compiled into this
//! process, receives nothing but the phase context and the configuration value,
//! and is held to the two ceilings by the same wrapper.

use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use crate::domain::plugin_contract::SandboxLimits;

/// The 100 ms ceiling of one invocation, as the contract publishes it.
pub const MAX_INVOCATION_MILLIS: u64 = 100;

/// The 10 MB ceiling of one invocation, as the contract publishes it.
pub const MAX_INVOCATION_MEMORY_BYTES: usize = 10 * 1024 * 1024;

/// The kind of implementation one invocation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationKind {
    /// A built-in implementation compiled into this process.
    Builtin,
    /// A stored custom plugin's Starlark source.
    CustomSource,
}

/// Why an invocation is refused before it is attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxRefusal {
    /// The limits cannot be enforced for this kind in this deployment.
    Unenforceable,
    /// The invocation's inputs already exceed a ceiling the invocation is held
    /// to, so it cannot run within its budget.
    OverBudget {
        /// Which ceiling the inputs exceed.
        reason: String,
    },
}

/// Why an invocation that ran was terminated and discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxFailure {
    /// The invocation exceeded its wall-clock ceiling.
    Timeout {
        /// The ceiling the invocation breached, in milliseconds.
        limit_millis: u64,
    },
    /// The invocation raised an error, caught at the boundary.
    Raised,
}

// @cpt-dod:cpt-cf-oagw-dod-starlark-sandbox:p1

/// Confirms before execution that the invocation can be held to the limits.
///
/// # Errors
///
/// Returns [`SandboxRefusal::Unenforceable`] for a custom source, because no
/// interpreter exists in this deployment to hold the four prohibitions and the
/// two ceilings against, and [`SandboxRefusal::OverBudget`] when the inputs the
/// invocation would already receive exceed its memory ceiling.
pub fn admit(
    kind: InvocationKind,
    limits: &SandboxLimits,
    input_bytes: usize,
) -> Result<(), SandboxRefusal> {
    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-capabilities
    // A capability that cannot be removed is a limit that cannot be enforced,
    // and the plugin is not run. A built-in implementation grants none: the
    // invocation receives the phase context and the configuration value, and
    // nothing else, so there is no network handle, no file handle, and no
    // import table to take away.
    if kind == InvocationKind::CustomSource
        || limits.network_io
        || limits.file_io
        || limits.imports
    {
        return Err(SandboxRefusal::Unenforceable);
    }
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-capabilities

    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-limits
    // The two ceilings are applied to this invocation alone, so one plugin's
    // breach never consumes another's budget. The memory ceiling is checked
    // against what the invocation is about to be handed: inputs already above
    // it cannot run within the budget they are given.
    if input_bytes > limits.max_invocation_memory_bytes {
        return Err(SandboxRefusal::OverBudget {
            reason: format!(
                "the invocation's inputs are {input_bytes} bytes, above the {} the ceiling allows",
                limits.max_invocation_memory_bytes
            ),
        });
    }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-limits
}

/// Runs one invocation under the sandbox discipline.
///
/// # Errors
///
/// Returns [`SandboxFailure::Raised`] when the invocation panics, and
/// [`SandboxFailure::Timeout`] when it returned after its wall-clock ceiling;
/// either way every partial mutation the invocation performed is discarded,
/// because the caller receives the failure and not the value it produced.
pub fn invoke<T>(
    limits: &SandboxLimits,
    run: impl FnOnce() -> T,
) -> Result<T, SandboxFailure> {
    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-try
    // The invocation runs here, with the phase implementation the chain handed
    // over and nothing else in scope.
    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-run
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(run));
    let elapsed = started.elapsed();
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-run
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-try

    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-catch
    let value = outcome.map_err(|_| SandboxFailure::Raised)?;
    // A breach is never retried and never re-issued: the value the invocation
    // produced past its ceiling is discarded with the ceiling it breached.
    if elapsed > Duration::from_millis(limits.max_invocation_millis) {
        return Err(SandboxFailure::Timeout {
            limit_millis: limits.max_invocation_millis,
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-catch

    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-catch-handle
    // The termination is the catch's answer: the invocation is ended, every
    // partial mutation it performed is discarded with the value it was
    // producing, and the failure is reported to
    // `cpt-cf-oagw-algo-chain-execute`, which answers 502 for it.
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-catch-handle

    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-else
    // The ELSE of the catch: the invocation ran inside both ceilings and
    // raised nothing.
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-else

    // @cpt-begin:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-return
    // RETURN the verdict or mutation, which the chain applies in the composed
    // order.
    Ok(value)
    // @cpt-end:cpt-cf-oagw-algo-starlark-sandbox:p1:inst-sandbox-return
}

/// Whether the sandbox limits are enforced for one invocation kind.
#[must_use]
pub fn enforceable(kind: InvocationKind, limits: &SandboxLimits) -> bool {
    admit(kind, limits, 0).is_ok()
}

/// The ceiling the contract publishes, when a caller needs the number it holds.
#[must_use]
pub fn published() -> SandboxLimits {
    crate::domain::plugin_contract::SANDBOX_LIMITS
}
