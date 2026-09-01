//! Conversion-request domain model types — internal storage / saga shapes.
//!
//! Pure Rust value types that express the dual-consent conversion
//! lifecycle independently of storage and transport. The `SeaORM` entity
//! lives in [`crate::infra::storage::entity::conversion_requests`].
//! Public input / output DTOs (request bodies, response envelopes) for
//! the eventual REST surface live on `account_management_sdk` and are
//! not duplicated here.
//!
//! What stays in this gear:
//!
//! * [`ConversionStatus`] — internal 5-variant lifecycle
//!   (`pending`, `approved`, `cancelled`, `rejected`, `expired`).
//! * [`ConversionSide`] — initiator/counterparty side discriminator
//!   (`child` vs `parent`).
//! * [`TargetMode`] — target mode the conversion will land on
//!   (`managed` vs `self_managed`).
//! * [`ConversionRequest`] — full storage row exposed by the repo,
//!   including the four optional per-transition audit comment columns
//!   (`requested_comment`, `approved_comment`, `cancelled_comment`,
//!   `rejected_comment`).
//! * [`NewConversionRequest`] — repo-level insert input for
//!   `insert_pending`, including the optional `requested_comment`
//!   stamped at request time.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Lifecycle status of a [`ConversionRequest`].
///
/// Encoded as `SMALLINT` at the DB layer per the `m0004` migration:
/// `0=Pending, 1=Approved, 2=Cancelled, 3=Rejected, 4=Expired`. Only
/// `Pending` is non-terminal; the four resolved variants are terminal
/// and may only enter the row through the role-gated transitions
/// validated by
/// [`crate::domain::conversion::state_machine::validate_transition`].
// @cpt-begin:cpt-cf-account-management-state-managed-self-managed-modes-conversion-request:p1:inst-state-conversion-status-domain
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConversionStatus {
    Pending,
    Approved,
    Cancelled,
    Rejected,
    Expired,
}
// @cpt-end:cpt-cf-account-management-state-managed-self-managed-modes-conversion-request:p1:inst-state-conversion-status-domain

impl ConversionStatus {
    /// Numeric `SMALLINT` encoding used by the DB schema.
    #[must_use]
    pub const fn as_smallint(self) -> i16 {
        match self {
            Self::Pending => 0,
            Self::Approved => 1,
            Self::Cancelled => 2,
            Self::Rejected => 3,
            Self::Expired => 4,
        }
    }

    /// Parse from `SMALLINT`. Returns `None` for any value outside the
    /// documented `{0, 1, 2, 3, 4}` domain.
    #[must_use]
    pub const fn from_smallint(value: i16) -> Option<Self> {
        match value {
            0 => Some(Self::Pending),
            1 => Some(Self::Approved),
            2 => Some(Self::Cancelled),
            3 => Some(Self::Rejected),
            4 => Some(Self::Expired),
            _ => None,
        }
    }

    /// Stable lowercase ASCII label used in audit payloads, structured
    /// logs, and the `InvalidActorForTransition` error envelope.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }

    /// `true` iff the status is `Pending` — the only non-terminal state.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }

    /// `true` iff the status is one of the four terminal states
    /// (`Approved`, `Cancelled`, `Rejected`, `Expired`).
    #[must_use]
    pub const fn is_resolved(self) -> bool {
        matches!(
            self,
            Self::Approved | Self::Cancelled | Self::Rejected | Self::Expired
        )
    }
}

/// Discriminates which side of the dual-consent pair is acting on a
/// [`ConversionRequest`]: the converting tenant itself (`Child`) or its
/// parent (`Parent`). Encoded as `SMALLINT` at the DB layer per the
/// `m0004` migration: `0=Child, 1=Parent`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConversionSide {
    Child,
    Parent,
}

impl ConversionSide {
    /// Numeric `SMALLINT` encoding used by the DB schema.
    #[must_use]
    pub const fn as_smallint(self) -> i16 {
        match self {
            Self::Child => 0,
            Self::Parent => 1,
        }
    }

    /// Parse from `SMALLINT`. Returns `None` for any value outside the
    /// documented `{0, 1}` domain.
    #[must_use]
    pub const fn from_smallint(value: i16) -> Option<Self> {
        match value {
            0 => Some(Self::Child),
            1 => Some(Self::Parent),
            _ => None,
        }
    }

    /// Stable lowercase ASCII label used in audit payloads and the
    /// `InvalidActorForTransition` error envelope.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Child => "child",
            Self::Parent => "parent",
        }
    }
}

/// Target mode the conversion will land on if approved. Encoded as
/// `SMALLINT` at the DB layer per the `m0004` migration:
/// `0=Managed, 1=SelfManaged`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TargetMode {
    Managed,
    SelfManaged,
}

impl TargetMode {
    /// Numeric `SMALLINT` encoding used by the DB schema.
    #[must_use]
    pub const fn as_smallint(self) -> i16 {
        match self {
            Self::Managed => 0,
            Self::SelfManaged => 1,
        }
    }

    /// Parse from `SMALLINT`. Returns `None` for any value outside the
    /// documented `{0, 1}` domain.
    #[must_use]
    pub const fn from_smallint(value: i16) -> Option<Self> {
        match value {
            0 => Some(Self::Managed),
            1 => Some(Self::SelfManaged),
            _ => None,
        }
    }

    /// Stable lowercase ASCII label used in audit payloads and the
    /// `InvalidActorForTransition` error envelope.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::SelfManaged => "self_managed",
        }
    }
}

/// Snapshot of a `conversion_requests` row as returned by
/// [`crate::domain::conversion::repo::ConversionRepo`].
///
/// Matches the column set declared by `m0004_create_conversion_requests`
/// 1:1 with the `SMALLINT`-encoded columns lifted into their domain
/// enums.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionRequest {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub child_tenant_name: String,
    pub initiator_side: ConversionSide,
    pub target_mode: TargetMode,
    pub status: ConversionStatus,
    pub requested_by: Uuid,
    pub approved_by: Option<Uuid>,
    pub cancelled_by: Option<Uuid>,
    pub rejected_by: Option<Uuid>,
    pub requested_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub expires_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
    /// Optional caller-supplied rationale captured at request time
    /// (`request_conversion`). The DB-side `m0006` CHECK pins length to
    /// `1..=1000`; absent comments stay `None`.
    pub requested_comment: Option<String>,
    /// Optional approver rationale captured on the
    /// `pending -> approved` transition. Same length contract as
    /// [`Self::requested_comment`].
    pub approved_comment: Option<String>,
    /// Optional canceller rationale captured on the
    /// `pending -> cancelled` transition. Same length contract.
    pub cancelled_comment: Option<String>,
    /// Optional rejecter rationale captured on the
    /// `pending -> rejected` transition. Same length contract.
    pub rejected_comment: Option<String>,
}

/// Repo-level insert input for
/// [`crate::domain::conversion::repo::ConversionRepo::insert_pending`].
///
/// Carries the immutable fields populated at request-creation time. The
/// resolved-actor fields (`approved_by`, `cancelled_by`, `rejected_by`,
/// `resolved_at`) and the lifecycle column `deleted_at` are stamped by
/// later transitions and are therefore not part of the insert input.
///
/// `requested_at` and `expires_at` are both supplied by the caller (the
/// service layer's `now_fn` clock seam) so the repo never reaches for a
/// wall-clock of its own. This keeps test reproducibility: a fixed
/// `now_fn` in unit tests pins both timestamps deterministically, and
/// the production `insert_pending` is engine-side a verbatim write.
#[domain_model]
#[derive(Debug, Clone)]
pub struct NewConversionRequest {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub child_tenant_name: String,
    pub initiator_side: ConversionSide,
    pub target_mode: TargetMode,
    pub requested_by: Uuid,
    pub requested_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    /// Optional caller-supplied rationale stamped on
    /// `conversion_requests.requested_comment`. The service layer
    /// enforces `1..=1000` chars before the INSERT; the DB-side
    /// `m0006` CHECK is defence-in-depth.
    pub requested_comment: Option<String>,
}
