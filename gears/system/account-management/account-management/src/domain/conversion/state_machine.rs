//! `ConversionRequest` state-machine guard.
//!
//! Pure, side-effect-free validation function consumed by the service
//! layer (and re-applied as defence-in-depth by the repo-impl) before
//! any DB write. Encodes the role-per-transition matrix from FEATURE
//! `managed-self-managed-modes` §4 / `DoD` `Dual-Consent Actor Discipline`:
//!
//! | Transition             | Role          | `caller_side` rule              |
//! |------------------------|---------------|---------------------------------|
//! | `pending -> approved`  | counterparty  | `caller_side != initiator_side` |
//! | `pending -> cancelled` | initiator     | `caller_side == initiator_side` |
//! | `pending -> rejected`  | counterparty  | `caller_side != initiator_side` |
//! | `pending -> expired`   | system        | `caller_side` MUST be `None`    |
//!
//! The state check (`current == Pending`) precedes the role check so a
//! caller who happens to satisfy the role rule for a request that has
//! already resolved still surfaces [`DomainError::AlreadyResolved`]
//! rather than [`DomainError::InvalidActorForTransition`]. This ordering
//! is required by `DoD` `Dual-Consent Actor Discipline` and exercised by
//! the unit tests in this gear.

use crate::domain::conversion::model::{ConversionSide, ConversionStatus};
use crate::domain::error::DomainError;

/// Sentinel string used in the `caller_side` field of
/// [`DomainError::InvalidActorForTransition`] when the caller passed a
/// concrete side but the transition required `None` (`pending -> expired`
/// is the only such case). Plain ASCII so it round-trips through every
/// canonical-error envelope without re-encoding.
const SYSTEM_PLACEHOLDER: &str = "system";

// @cpt-begin:cpt-cf-account-management-state-managed-self-managed-modes-conversion-request:p1:inst-state-conversion-transition-guard
/// Validate a `(current, target, caller_side, initiator_side)` tuple
/// against the role-per-transition matrix.
///
/// # Errors
///
/// * [`DomainError::AlreadyResolved`] when `current` is not
///   [`ConversionStatus::Pending`] — the state check precedes the role
///   check per `DoD` `Dual-Consent Actor Discipline`.
/// * [`DomainError::InvalidActorForTransition`] when the role rule for
///   the requested target is not satisfied. The error's
///   `attempted_status` carries the lowercase ASCII label of the target
///   ([`ConversionStatus::as_str`]); `caller_side` carries the lowercase
///   ASCII label of the caller side when one was supplied
///   ([`ConversionSide::as_str`]) or the literal `"system"` when the
///   caller passed `None` for a transition that required a concrete
///   side. For `pending -> expired`, where `None` is required, a
///   `Some(side)` caller surfaces the side label for the audit trail.
pub fn validate_transition(
    current: ConversionStatus,
    target: ConversionStatus,
    caller_side: Option<ConversionSide>,
    initiator_side: ConversionSide,
) -> Result<(), DomainError> {
    // State check precedes role check. A request that has already
    // resolved is rejected here regardless of role rule.
    if !matches!(current, ConversionStatus::Pending) {
        return Err(DomainError::AlreadyResolved);
    }

    match target {
        ConversionStatus::Approved | ConversionStatus::Rejected => {
            // Counterparty-only: caller MUST supply a concrete side and
            // it MUST differ from the initiator.
            match caller_side {
                Some(side) if side != initiator_side => Ok(()),
                _ => Err(invalid_actor(target, caller_side)),
            }
        }
        ConversionStatus::Cancelled => {
            // Initiator-only: caller MUST supply a concrete side and it
            // MUST match the initiator.
            match caller_side {
                Some(side) if side == initiator_side => Ok(()),
                _ => Err(invalid_actor(target, caller_side)),
            }
        }
        ConversionStatus::Expired => {
            // System-only: caller_side MUST be `None`. A concrete caller
            // side here means a non-system actor reached the reaper
            // path, which is a programmer error.
            if caller_side.is_none() {
                Ok(())
            } else {
                Err(invalid_actor(target, caller_side))
            }
        }
        ConversionStatus::Pending => {
            // Re-pending is never a legal transition; the only legal
            // path into `pending` is `insert_pending`, which never
            // reaches this guard. Reaching this arm is therefore a
            // programmer error (a service-layer caller asked for a
            // transition shape the FEATURE doc does not define), not
            // a caller-side actor mismatch — surface as `Internal`
            // so it lands on `am.domain` / 500 instead of an
            // authorization-flavoured 400.
            Err(DomainError::Internal {
                diagnostic: format!(
                    "validate_transition: target=Pending is not a legal transition \
                     (caller_side={caller_side:?}, initiator_side={initiator_side:?})"
                ),
                cause: None,
            })
        }
    }
}
// @cpt-end:cpt-cf-account-management-state-managed-self-managed-modes-conversion-request:p1:inst-state-conversion-transition-guard

/// Build a [`DomainError::InvalidActorForTransition`] populated with the
/// canonical lowercase ASCII labels for `target` and `caller_side`.
fn invalid_actor(target: ConversionStatus, caller_side: Option<ConversionSide>) -> DomainError {
    let caller_side_str = match caller_side {
        Some(side) => side.as_str().to_owned(),
        None => SYSTEM_PLACEHOLDER.to_owned(),
    };
    DomainError::InvalidActorForTransition {
        attempted_status: target.as_str().to_owned(),
        caller_side: caller_side_str,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "state_machine_tests.rs"]
mod state_machine_tests;
