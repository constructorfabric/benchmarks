//! The `enabled` state machine of the management surface
//! (`cpt-cf-oagw-state-resource-enabled`).
//!
//! Two states, `Enabled` and `Disabled`, entered at create time from the state
//! the body carries and moved afterwards by exactly the two `PUT` transitions
//! the machine declares. Existence and deletion stay owned by the domain model,
//! which enters `Deleted` without this machine taking part.

/// The state the `enabled` flag of an upstream or a route puts the resource in
/// (`cpt-cf-oagw-state-resource-enabled`).
///
/// A [`crate::domain::model::Plugin`] carries no `enabled` field, so it is never
/// an input of this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnabledState {
    /// `enabled: true`, the state a resource enters when the flag is omitted or
    /// set to `true`.
    Enabled,
    /// `enabled: false`, the state a resource enters when the body sets `false`.
    Disabled,
}

impl EnabledState {
    /// The create-time entry: the state the create body carries, with the
    /// omitted flag taking the `true` default of
    /// `cpt-cf-oagw-fr-enable-disable` and of
    /// `schemas/upstream.v1.schema.json`.
    #[must_use]
    pub fn on_create(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }

    /// The `enabled` flag the state stores and returns.
    #[must_use]
    pub const fn as_bool(self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// Applies one of the two `PUT` transitions of the machine.
    ///
    /// A write that carries the value the flag already holds is a no-op and
    /// leaves the state unchanged; any transition other than the two declared
    /// ones is refused, and this function never reads or writes a field other
    /// than `enabled`.
    // @cpt-begin:cpt-cf-oagw-state-resource-enabled:p1:inst-en-01
    // The `Enabled` to `Disabled` transition is a write of `enabled: false`
    // from the owning tenant.
    #[must_use]
    pub const fn on_put(self, enabled: bool) -> Option<Self> {
        // @cpt-begin:cpt-cf-oagw-state-resource-enabled:p1:inst-en-02
        // The `Disabled` to `Enabled` transition is a write of `enabled: true`
        // on the same resource; writing the value the flag already holds is the
        // no-op that leaves the state unchanged.
        match (self, enabled) {
            (Self::Enabled, false) => Some(Self::Disabled),
            (Self::Disabled, true) => Some(Self::Enabled),
            (Self::Enabled, true) | (Self::Disabled, false) => Some(self),
        }
        // @cpt-end:cpt-cf-oagw-state-resource-enabled:p1:inst-en-02
    }
    // @cpt-end:cpt-cf-oagw-state-resource-enabled:p1:inst-en-01
}

/// The `enabled` default of the upstream schema and of the route field-set
/// extension: the value a payload that omits the flag is read as.
pub const ENABLED_DEFAULT: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    /// The create-time entry of the machine is the state the body carries.
    #[test]
    fn the_create_time_entry_is_the_state_the_body_carries() {
        assert_eq!(EnabledState::on_create(true), EnabledState::Enabled);
        assert_eq!(EnabledState::on_create(false), EnabledState::Disabled);
        // An omitted flag takes the `true` default, which the DTO layer has
        // already applied to the aggregate it builds.
        assert_eq!(
            EnabledState::on_create(ENABLED_DEFAULT),
            EnabledState::Enabled
        );
    }

    /// `cpt-cf-oagw-state-resource-enabled` transition 1: `Enabled` to
    /// `Disabled`.
    #[test]
    fn a_put_writing_false_moves_an_enabled_resource_to_disabled() {
        assert_eq!(
            EnabledState::Enabled.on_put(false),
            Some(EnabledState::Disabled)
        );
    }

    /// `cpt-cf-oagw-state-resource-enabled` transition 2: `Disabled` to
    /// `Enabled`.
    #[test]
    fn a_put_writing_true_moves_a_disabled_resource_to_enabled() {
        assert_eq!(
            EnabledState::Disabled.on_put(true),
            Some(EnabledState::Enabled)
        );
    }

    /// A write of the value the flag already holds is a no-op.
    #[test]
    fn a_put_writing_the_value_the_flag_already_holds_is_a_no_op() {
        assert_eq!(
            EnabledState::Enabled.on_put(true),
            Some(EnabledState::Enabled)
        );
        assert_eq!(
            EnabledState::Disabled.on_put(false),
            Some(EnabledState::Disabled)
        );
    }

    /// The state stores and returns the flag.
    #[test]
    fn the_state_returns_the_flag_it_carries() {
        assert!(EnabledState::Enabled.as_bool());
        assert!(!EnabledState::Disabled.as_bool());
    }
}
