use super::*;
use crate::domain::conversion::model::{ConversionSide, ConversionStatus};
use crate::domain::error::DomainError;

// ---- Happy paths ---------------------------------------------------

#[test]
fn pending_to_approved_by_counterparty_succeeds_when_initiator_is_child() {
    let result = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Approved,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    );
    assert!(matches!(result, Ok(())));
}

#[test]
fn pending_to_approved_by_counterparty_succeeds_when_initiator_is_parent() {
    let result = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Approved,
        Some(ConversionSide::Child),
        ConversionSide::Parent,
    );
    assert!(matches!(result, Ok(())));
}

#[test]
fn pending_to_rejected_by_counterparty_succeeds() {
    let result = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Rejected,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    );
    assert!(matches!(result, Ok(())));
}

#[test]
fn pending_to_cancelled_by_initiator_succeeds() {
    let result = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Cancelled,
        Some(ConversionSide::Child),
        ConversionSide::Child,
    );
    assert!(matches!(result, Ok(())));
}

#[test]
fn pending_to_expired_with_no_caller_side_succeeds() {
    let result = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Expired,
        None,
        ConversionSide::Child,
    );
    assert!(matches!(result, Ok(())));
}

// ---- Role-rule violations ------------------------------------------

#[test]
fn pending_to_approved_by_initiator_returns_invalid_actor_for_transition() {
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Approved,
        Some(ConversionSide::Child),
        ConversionSide::Child,
    )
    .expect_err("initiator must not be able to approve their own request");
    match err {
        DomainError::InvalidActorForTransition {
            attempted_status,
            caller_side,
        } => {
            assert_eq!(attempted_status, "approved");
            assert_eq!(caller_side, "child");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn pending_to_rejected_by_initiator_returns_invalid_actor_for_transition() {
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Rejected,
        Some(ConversionSide::Parent),
        ConversionSide::Parent,
    )
    .expect_err("initiator must not be able to reject their own request");
    match err {
        DomainError::InvalidActorForTransition {
            attempted_status,
            caller_side,
        } => {
            assert_eq!(attempted_status, "rejected");
            assert_eq!(caller_side, "parent");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn pending_to_cancelled_by_counterparty_returns_invalid_actor_for_transition() {
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Cancelled,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    )
    .expect_err("counterparty must not be able to cancel the initiator's request");
    match err {
        DomainError::InvalidActorForTransition {
            attempted_status,
            caller_side,
        } => {
            assert_eq!(attempted_status, "cancelled");
            assert_eq!(caller_side, "parent");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn pending_to_approved_with_no_caller_side_returns_invalid_actor_for_transition() {
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Approved,
        None,
        ConversionSide::Child,
    )
    .expect_err("approval requires a concrete counterparty caller side");
    match err {
        DomainError::InvalidActorForTransition {
            attempted_status,
            caller_side,
        } => {
            assert_eq!(attempted_status, "approved");
            assert_eq!(caller_side, "system");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn pending_to_cancelled_with_no_caller_side_returns_invalid_actor_for_transition() {
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Cancelled,
        None,
        ConversionSide::Child,
    )
    .expect_err("cancellation requires a concrete initiator caller side");
    match err {
        DomainError::InvalidActorForTransition {
            attempted_status,
            caller_side,
        } => {
            assert_eq!(attempted_status, "cancelled");
            assert_eq!(caller_side, "system");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn pending_to_expired_with_caller_side_returns_invalid_actor_for_transition() {
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Expired,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    )
    .expect_err("expiry is system-driven; a concrete caller side is illegal");
    match err {
        DomainError::InvalidActorForTransition {
            attempted_status,
            caller_side,
        } => {
            assert_eq!(attempted_status, "expired");
            // Side label is preserved for audit trail.
            assert_eq!(caller_side, "parent");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

// ---- Already-resolved precedes role check --------------------------

#[test]
fn from_approved_to_anything_returns_already_resolved() {
    // Use a caller that *would* satisfy the role rule for `Cancelled`
    // (initiator side). The state check still wins.
    let err = validate_transition(
        ConversionStatus::Approved,
        ConversionStatus::Cancelled,
        Some(ConversionSide::Child),
        ConversionSide::Child,
    )
    .expect_err("already-resolved requests cannot transition");
    assert!(matches!(err, DomainError::AlreadyResolved));
}

#[test]
fn from_cancelled_to_anything_returns_already_resolved() {
    let err = validate_transition(
        ConversionStatus::Cancelled,
        ConversionStatus::Approved,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    )
    .expect_err("already-resolved requests cannot transition");
    assert!(matches!(err, DomainError::AlreadyResolved));
}

#[test]
fn from_rejected_to_anything_returns_already_resolved() {
    let err = validate_transition(
        ConversionStatus::Rejected,
        ConversionStatus::Approved,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    )
    .expect_err("already-resolved requests cannot transition");
    assert!(matches!(err, DomainError::AlreadyResolved));
}

#[test]
fn from_expired_to_anything_returns_already_resolved() {
    let err = validate_transition(
        ConversionStatus::Expired,
        ConversionStatus::Approved,
        Some(ConversionSide::Parent),
        ConversionSide::Child,
    )
    .expect_err("already-resolved requests cannot transition");
    assert!(matches!(err, DomainError::AlreadyResolved));
}

// ---- target == Pending is never legal ------------------------------

#[test]
fn to_pending_target_is_rejected_as_internal_programmer_error() {
    // Re-pending is not a legal transition; the only legal path
    // into `Pending` is `insert_pending`, which never reaches this
    // guard. A caller hitting this branch is a programmer error,
    // so the helper surfaces `Internal` (500) — NOT
    // `InvalidActorForTransition` (which would be a 400 about a
    // legitimate caller-side mistake).
    let err = validate_transition(
        ConversionStatus::Pending,
        ConversionStatus::Pending,
        Some(ConversionSide::Child),
        ConversionSide::Child,
    )
    .expect_err("re-pending is not a legal transition");
    match err {
        DomainError::Internal { diagnostic, .. } => {
            assert!(
                diagnostic.contains("target=Pending"),
                "diagnostic must name the bad target; got: {diagnostic}"
            );
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}
