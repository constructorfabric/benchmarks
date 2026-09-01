//! User Groups feature (DECOMPOSITION §2.6).
//!
//! Delegates all user-group state to the Resource Group gear.
//! AM owns two thin touchpoints:
//!
//! 1. Idempotent registration of **two** chained RG type schemas
//!    during gear init ([`register_user_group_types`]):
//!
//!    - [`USER_MEMBERSHIP_TYPE`] -- the AM-user member handle. A
//!      type-registry-only entry; AM users live in AM's tables +
//!      `IdP`, never as RG groups. RG needs the row in `gts_type` to
//!      let `add_membership` resolve the resource type.
//!    - [`USER_GROUP_TYPE_CODE`] -- the user-group container. Groups
//!      of this type are AM-owned RG groups (tenant-scoped) whose
//!      `allowed_membership_types` lists [`USER_MEMBERSHIP_TYPE`].
//!
//!    Registration order matters: the member handle MUST land before
//!    the container, otherwise the container's
//!    `resolve_ids(allowed_membership_types)` step fails closed.
//! 2. A [`TenantHardDeleteHook`] that triggers RG-side cascade cleanup
//!    of the tenant's user-group subtree before the `tenants` row is
//!    removed ([`build_cascade_cleanup_hook`]).

pub(crate) mod registration;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "registration_tests.rs"]
mod registration_tests;

pub(crate) mod cascade;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "cascade_tests.rs"]
mod cascade_tests;

pub(crate) use cascade::build_cascade_cleanup_hook;
pub(crate) use registration::register_user_group_types;

/// Re-export of [`account_management_sdk::gts::USER_GROUP_RG_TYPE_CODE`]
/// under the legacy AM-internal name `USER_GROUP_TYPE_CODE`. The RG
/// type-registry handle for the AM user-group container.
pub const USER_GROUP_TYPE_CODE: &str = account_management_sdk::gts::USER_GROUP_RG_TYPE_CODE;

/// Re-export of [`account_management_sdk::gts::USER_RG_TYPE_CODE`]
/// under the legacy AM-internal name `USER_MEMBERSHIP_TYPE`. The RG
/// type-registry handle for the AM user member type, used as
/// `resource_type` when adding / removing AM-user memberships in
/// user-groups.
pub(crate) const USER_MEMBERSHIP_TYPE: &str = account_management_sdk::gts::USER_RG_TYPE_CODE;

// System-actor factories live in [`crate::domain::system_actor`] and
// are imported directly at each call site (no crate-root convenience
// re-export). Routing every legitimate use case through a named
// factory makes "where does AM elevate to system?" trivially
// auditable by grep, and the per-site `tracing` log emitted by each
// factory gives the audit pipeline a wire signal. See the
// `system_actor` gear docs for the full rationale.
