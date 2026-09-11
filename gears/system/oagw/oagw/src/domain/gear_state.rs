//! `GearFoundationState` — the provisioning state machine of the gear
//! (`cpt-cf-oagw-state-gear-foundation-lifecycle`).

use serde::{Deserialize, Serialize};

/// Lifecycle of the `oagw` gear foundation.
///
/// `StartupFailed` is terminal: the runtime aborts startup, so no later
/// feature ever observes a half-initialized gear.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GearFoundationState {
    /// The gear is not registered with the runtime yet.
    Unregistered,
    /// The init hook completed with a validated `OagwConfig`.
    Configured,
    /// Every catalogue entry is registered and the registry catalogue is in
    /// its ready phase.
    TypeCatalogProvisioned,
    /// The gear reports readiness; it serves only the router mount point,
    /// which carries no routes in this feature.
    Ready,
    /// Terminal: configuration loading, validation, or a catalogue entry
    /// failed.
    StartupFailed,
}

impl GearFoundationState {
    /// `true` when no declared transition leaves this state: `Ready` is the
    /// end of the successful path and `StartupFailed` the end of the failing
    /// one.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        // @cpt-begin:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-terminal
        matches!(self, Self::Ready | Self::StartupFailed)
        // @cpt-end:cpt-cf-oagw-state-gear-foundation-lifecycle:p1:inst-state-terminal
    }

    /// Whether the transition `self -> next` is one of the declared
    /// transitions.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Unregistered, Self::Configured)
                | (Self::Unregistered, Self::StartupFailed)
                | (Self::Configured, Self::TypeCatalogProvisioned)
                | (Self::Configured, Self::StartupFailed)
                | (Self::TypeCatalogProvisioned, Self::Ready)
        )
    }

    /// Applies a transition, keeping the state machine honest.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidTransition`] naming both endpoints when the
    /// transition is not one of the declared ones.
    pub fn transition(self, next: Self) -> Result<Self, InvalidTransition> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

impl std::fmt::Display for GearFoundationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rendered = match self {
            Self::Unregistered => "unregistered",
            Self::Configured => "configured",
            Self::TypeCatalogProvisioned => "type_catalog_provisioned",
            Self::Ready => "ready",
            Self::StartupFailed => "startup_failed",
        };
        f.write_str(rendered)
    }
}

/// A transition outside the declared set of the state machine.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid gear-foundation transition from {from} to {to}")]
pub struct InvalidTransition {
    /// The state the transition started from.
    pub from: GearFoundationState,
    /// The state the transition attempted to reach.
    pub to: GearFoundationState,
}
