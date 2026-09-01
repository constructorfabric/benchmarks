//! Static `IdP` Plugin
//!
//! Permissive, in-memory echo implementation of
//! [`account_management_sdk::IdpPluginClient`] for development and E2E
//! environments. Every operation succeeds with a deterministic payload
//! so Account Management's bootstrap saga and tenant lifecycle flows
//! can run end-to-end without a real `IdP`.
//!
//! Do NOT enable this plugin in production. The production fallback is
//! `account_management::infra::idp::NoopIdpProvider`, whose
//! `UnsupportedOperation` defaults force operators to wire a real
//! provider.
//!
//! ## Behaviour
//!
//! | Method | Behaviour |
//! |---|---|
//! | `provision_tenant` | Returns `Ok(IdpProvisionResult::new(Some(echo_metadata)))` — a deterministic JSON projection of the request (`tenant_id`, `tenant_name`, `tenant_type`, `root`/`child` target, optional `parent_id`, echoed `provisioning_metadata`). Pure function of the input so E2E suites can pin byte-for-byte equality; surfaces AM's `Some(metadata)` activation branch end-to-end. |
//! | `deprovision_tenant` | Returns `Ok(())`. |
//! | `provision_user` | Echoes the request as an `IdpUser` whose `id` is a deterministic `UUIDv5` derived from `(tenant_id, username)`, then records it in the per-tenant in-memory cache. |
//! | `deprovision_user` | Removes the matching row from the per-tenant cache; collapses removed-and-already-absent to `Ok(())` per the SDK contract. |
//! | `list_users` | Paginates the per-tenant cache, honouring the typed `OData` filter (`FilterNode<IdpUserFilterField>`) and order (`ODataOrderBy`) on the SPI request. Filtered point lookup (`$filter = id eq <uuid>`) returns either an empty page or a single-element page (the authoritative existence signal). Default order is `username ASC, id ASC` (with `id ASC` tiebreaker injected on caller-supplied orders too). Cursors are `toolkit_odata::CursorV1` key-tuple boundaries; mismatch between the cursor's encoded order and the request's effective order surfaces as `IdpUserOperationFailure::Rejected`, so a hostile / buggy client cannot smuggle state into the plugin. |
//!
//! ## State
//!
//! The plugin keeps a per-tenant `HashMap<user_id, IdpUser>` behind a
//! `parking_lot::Mutex`. State lives in-process and is dropped on
//! restart — matching the dev-only contract of every other
//! `static-*-plugin`. A real `IdP` provider would persist this state
//! upstream; AM consumes the same trait surface either way.
//!
//! ## Configuration
//!
//! ```yaml
//! gears:
//!   static-idp-plugin:
//!     config:
//!       vendor: "constructorfabric"
//!       priority: 100
//! ```
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod domain;
pub mod gear;

pub use gear::StaticIdpPlugin;
