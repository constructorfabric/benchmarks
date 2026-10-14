//! Turn mutations and reactions (feature `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`):
//! the retry, edit and delete of the latest terminal turn, the carry-forward of attachments and
//! tool usage, the summary invalidation, the mutation audit event, the mutation metrics, the
//! unstarted replacement turn failure and the like or dislike reactions on assistant messages.
//!
//! The pure rules live in the small modules (`request`, `preview`, `carry`, `summary`, `audit`,
//! `reactions`, `metrics`); `service` holds the steps of the flows over the persistence ports of
//! `store`, implemented by `infra::storage::mutation_store`.

pub mod audit;
pub mod carry;
pub mod metrics;
pub mod preview;
pub mod reactions;
pub mod request;
pub mod service;
pub mod store;
pub mod summary;

use toolkit_macros::domain_model;

use crate::domain::authz::Operation;

pub use audit::build_mutation_audit;
pub use carry::{CarriedAttachments, LinkedAttachment, carry_forward_attachments};
pub use metrics::{MutationMetrics, MutationResult, classify_rejection};
pub use preview::{check_latest, check_target, select_latest};
pub use reactions::{
    ReactionAction, StoredReaction, check_reaction_target, next_reaction, parse_reaction,
};
pub use request::{ValidatedEdit, validate_edit_content};
pub use service::{
    CommittedMutation, MutationPreview, PreparedMutation, ReactionService, TurnMutationService,
};
pub use store::{
    CommitCommand, ExistingReserve, MutationReads, MutationStore, MutationTurn, ReactionStore,
    UnstartedFailure,
};
pub use summary::frontier_covers;

/// The three mutations of the latest terminal turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOp {
    Retry,
    Edit,
    Delete,
}

impl MutationOp {
    /// The metric label and audit name: `retry`, `edit` or `delete`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }

    /// The authorization operation (`retry_turn`, `edit_turn` or `delete_turn`).
    #[must_use]
    pub const fn operation(self) -> Operation {
        match self {
            Self::Retry => Operation::RetryTurn,
            Self::Edit => Operation::EditTurn,
            Self::Delete => Operation::DeleteTurn,
        }
    }

    /// Whether the mutation creates a replacement turn (retry and edit).
    #[must_use]
    pub const fn replaces(self) -> bool {
        !matches!(self, Self::Delete)
    }
}

#[cfg(test)]
#[path = "mutations/request_tests.rs"]
mod request_tests;

#[cfg(test)]
#[path = "mutations/preview_tests.rs"]
mod preview_tests;

#[cfg(test)]
#[path = "mutations/carry_tests.rs"]
mod carry_tests;

#[cfg(test)]
#[path = "mutations/summary_tests.rs"]
mod summary_tests;

#[cfg(test)]
#[path = "mutations/audit_tests.rs"]
mod audit_tests;

#[cfg(test)]
#[path = "mutations/reactions_tests.rs"]
mod reactions_tests;

#[cfg(test)]
#[path = "mutations/metrics_tests.rs"]
mod metrics_tests;

#[cfg(test)]
#[path = "mutations/preflight_tests.rs"]
mod preflight_tests;

#[cfg(test)]
#[path = "mutations/setup_tests.rs"]
mod setup_tests;

#[cfg(test)]
#[path = "mutations/reaction_service_tests.rs"]
mod reaction_service_tests;
