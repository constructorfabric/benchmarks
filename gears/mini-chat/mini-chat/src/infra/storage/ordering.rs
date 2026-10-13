//! The strict total order of messages, `(created_at, id)` (DESIGN "`ContextPlan` Determinism and
//! Snapshot Boundary"), as query predicates.

use sea_orm::{ColumnTrait, Condition};
use time::OffsetDateTime;
use uuid::Uuid;

use super::entity::messages;

/// `(created_at, id) <= (at, id)`.
pub(super) fn at_or_before(created_at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.lt(created_at))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(created_at))
                .add(messages::Column::Id.lte(id)),
        )
}

/// `(created_at, id) > (at, id)`.
pub(super) fn after(created_at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.gt(created_at))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(created_at))
                .add(messages::Column::Id.gt(id)),
        )
}
