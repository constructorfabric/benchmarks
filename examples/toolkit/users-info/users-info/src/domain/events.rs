use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Transport-agnostic domain event.
#[domain_model]
#[derive(Debug, Clone)]
pub enum UserDomainEvent {
    Created { id: Uuid, at: OffsetDateTime },
    Updated { id: Uuid, at: OffsetDateTime },
    Deleted { id: Uuid, at: OffsetDateTime },
}
