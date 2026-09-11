//! Sharing-mode and permission decision — `cpt-cf-oagw-algo-sharing-mode-decision`.
//!
//! A descendant write that names a family an ancestor also configures is not
//! an ordinary write: the ancestor's sharing mode for that family decides
//! whether the descendant's value is its own configuration, overrides a base
//! the ancestor supplied, or is refused. This module answers that question
//! for every family a body carries, in one pass, and refuses the whole write
//! when any one family refuses, so a caller is never made to retry once per
//! blocked family.
//!
//! The four permissions the decision consults are the descendant override
//! permissions of DESIGN §3.2 — `oagw:upstream:bind`, `oagw:upstream:override_auth`,
//! `oagw:upstream:override_rate`, and `oagw:upstream:add_plugins`. They are
//! evaluated here, inside this feature's flows, and denied by default: a
//! permission the calling token does not literally carry is a permission the
//! caller does not hold, and an unknown permission literal is never held
//! either. CORS carries no permission at all, so for that family the sharing
//! mode alone decides.

// @cpt-dod:cpt-cf-oagw-dod-sharing-mode-decision:p1
// @cpt-dod:cpt-cf-oagw-dod-descendant-override-permissions:p1

use toolkit_security::SecurityContext;

use crate::control_plane::effective::strictest;
use crate::domain::effective::{AncestorBinding, Family};
use crate::domain::upstream::SharingMode;
use crate::gts;

/// The four descendant override permissions one token may carry, resolved from
/// the token's own scopes.
///
/// The platform grants and stores no permission of its own for these four, so
/// the set is exactly what the bearer token asserts: a scope is held when the
/// token names it, and the platform's unrestricted sentinel — a token whose
/// scope list is `["*"]` — names every one of them. Every other token denies
/// by default, which is the posture DESIGN §3.2 states for a descendant that
/// holds none of them: it resolves, proxies, and inherits, and cannot change
/// what it inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverridePermissions {
    /// `oagw:upstream:bind` — the bind-style create against an ancestor's alias.
    bind: bool,
    /// `oagw:upstream:override_auth` — the auth override of an `inherit` family.
    auth: bool,
    /// `oagw:upstream:override_rate` — an own rate limit under the minimum.
    rate: bool,
    /// `oagw:upstream:add_plugins` — appending own items to an inherited chain.
    plugins: bool,
}

/// The platform's unrestricted-scope sentinel, documented on
/// `SecurityContext::token_scopes` as first-party and unrestricted.
const UNRESTRICTED: &str = "*";

impl OverridePermissions {
    /// Reads the four permissions off one authenticated subject.
    #[must_use]
    pub fn of(context: &SecurityContext) -> Self {
        let scopes = context.token_scopes();
        let holds = |permission: &str| {
            scopes
                .iter()
                .any(|scope| scope == permission || scope == UNRESTRICTED)
        };
        Self {
            bind: holds(gts::PERMISSION_BIND),
            auth: holds(gts::PERMISSION_OVERRIDE_AUTH),
            rate: holds(gts::PERMISSION_OVERRIDE_RATE),
            plugins: holds(gts::PERMISSION_ADD_PLUGINS),
        }
    }

    /// The set that holds none of the four: the deny-by-default answer for a
    /// token whose scope list asserts nothing this feature recognizes.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            bind: false,
            auth: false,
            rate: false,
            plugins: false,
        }
    }

    /// Whether the token carries one permission literal.
    ///
    /// A literal outside the four is never held: this feature denies by
    /// default, and an unknown permission is not one it can grant.
    #[must_use]
    pub fn holds(&self, permission: &str) -> bool {
        match permission {
            gts::PERMISSION_BIND => self.bind,
            gts::PERMISSION_OVERRIDE_AUTH => self.auth,
            gts::PERMISSION_OVERRIDE_RATE => self.rate,
            gts::PERMISSION_ADD_PLUGINS => self.plugins,
            _ => false,
        }
    }
}

/// The decision one family received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// The descendant's value is its own configuration; no ancestor value
    /// exists to inherit and no override permission is consumed.
    Own,
    /// The ancestor's value is the base and the body's value overrides it.
    InheritBase,
    /// The ancestor's value is applied in every resolution and nothing of the
    /// body's reaches the row.
    Forced,
}

impl DecisionKind {
    /// Whether the decision lets the body's value reach the row.
    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(self, Self::Own | Self::InheritBase)
    }
}

/// Why one family refused the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 403: the family's override permission is not held, so the descendant
    /// uses the ancestor's value as-is.
    Permission {
        /// The family whose override permission was not held.
        family: Family,
    },
    /// 400: the ancestor marks the family `enforce` and the body carries a
    /// value for it.
    Enforced {
        /// The family the ancestor enforces.
        family: Family,
    },
}

impl Refusal {
    /// The family the refusal names.
    #[must_use]
    pub const fn family(self) -> Family {
        match self {
            Self::Permission { family } | Self::Enforced { family } => family,
        }
    }

    /// The override permission the refusal answers with, when it is a
    /// permission refusal.
    #[must_use]
    pub const fn permission(self) -> Option<&'static str> {
        match self {
            Self::Permission { family } => family.override_permission(),
            Self::Enforced { .. } => None,
        }
    }
}

/// One family's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyDecision {
    /// The family the decision answers.
    pub family: Family,
    /// The decision the ancestor's mode and the permission set produced.
    pub kind: DecisionKind,
}

/// The per-family answers of one write.
///
/// Only the families the body carries appear: a family the body omits is
/// written by nobody and takes no part in the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decisions {
    /// One decision per family the body carries.
    pub families: Vec<FamilyDecision>,
}

impl Decisions {
    /// The decision one carried family received.
    #[must_use]
    pub fn kind_of(&self, family: Family) -> Option<DecisionKind> {
        self.families
            .iter()
            .find(|decision| decision.family == family)
            .map(|decision| decision.kind)
    }

    /// Whether the body's value for one family may reach the row.
    ///
    /// A family the body does not carry is never written, so it answers
    /// `false` here as well: there is no value to write.
    #[must_use]
    pub fn writes(&self, family: Family) -> bool {
        self.kind_of(family).is_some_and(DecisionKind::writes)
    }

    /// Whether an ancestor forces one family, so nothing of the body's may
    /// reach the row.
    #[must_use]
    pub fn forced(&self, family: Family) -> bool {
        self.kind_of(family) == Some(DecisionKind::Forced)
    }
}

/// Decides every family a write body carries against the ancestor bindings the
/// chain walk resolved.
///
/// The refusal, when there is one, is the first in the order the feature fixes:
/// the permission 403 before any `enforce` 400, so a caller that lacks a
/// permission never learns which families its ancestor enforces.
///
/// # Errors
///
/// Returns the refusal of the highest-priority blocked family.
pub fn decide(
    ancestors: &[AncestorBinding],
    carried: &[Family],
    permissions: &OverridePermissions,
) -> Result<Decisions, Refusal> {
    // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-loop
    let mut families: Vec<FamilyDecision> = Vec::new();
    let mut refusals: Vec<Refusal> = Vec::new();
    for family in carried {
        // A family no ancestor contributes is the descendant's own
        // configuration whether the ancestor holds it `private` or not at all:
        // a `private` value is never carried into a binding, so both arrive
        // here as an empty contribution.
        let modes: Vec<SharingMode> = ancestors
            .iter()
            .filter(|binding| binding.contributes(*family))
            .map(|binding| binding.mode_of(*family))
            .collect();

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-noancestor-if
        let decided = if modes.is_empty() {
            // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-noancestor
            Ok(DecisionKind::Own)
            // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-noancestor
        } else {
            // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-row
            decide_row(*family, &modes, permissions)
            // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-row
        };
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-noancestor-if

        match decided {
            Ok(kind) => families.push(FamilyDecision {
                family: *family,
                kind,
            }),
            Err(refusal) => refusals.push(refusal),
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-loop

    // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-if
    if let Some(refusal) = first(refusals) {
        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-return
        return Err(refusal);
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-return
    }
    // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-if

    // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-return
    Ok(Decisions { families })
    // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-return
}

/// The first refusal in the order the feature fixes: the permission 403 before
/// any `enforce` 400.
fn first(refusals: Vec<Refusal>) -> Option<Refusal> {
    refusals
        .iter()
        .find(|refusal| matches!(refusal, Refusal::Permission { .. }))
        .copied()
        .or_else(|| refusals.into_iter().next())
}

/// Decides one family from the table, over the modes of every ancestor that
/// contributes it.
///
/// The strictest contributed mode decides: one `enforce` ancestor is enough to
/// refuse the value, and one `inherit` ancestor is enough to demand the
/// permission. A `private` contribution never reaches this row, because a
/// `private` family is never carried into a binding.
fn decide_row(
    family: Family,
    modes: &[SharingMode],
    permissions: &OverridePermissions,
) -> Result<DecisionKind, Refusal> {
    let mode = strictest(modes.iter().copied());
    match mode {
        SharingMode::Enforce => Err(Refusal::Enforced { family }),
        SharingMode::Inherit => match family.override_permission() {
            // CORS carries no permission, so its mode alone decides.
            None => Ok(DecisionKind::InheritBase),
            Some(permission) if permissions.holds(permission) => Ok(DecisionKind::InheritBase),
            Some(_) => Err(Refusal::Permission { family }),
        },
        // Unreachable from the walk: a `private` family contributes nothing.
        // Held as `own` so the table's first row stays total.
        SharingMode::Private => Ok(DecisionKind::Own),
    }
}
