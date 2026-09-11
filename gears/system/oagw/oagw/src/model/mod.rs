//! Domain/DTO model types for the `oagw` gear's Control-Plane resources.
//!
//! Modeled against `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` (Plugin has no schema file of its
//! own; it follows `DESIGN.md` §3.1's domain-model class diagram instead).
//! Only this feature's skeleton depends on these types today -- validation,
//! alias derivation, tenant scoping and persistence belong to
//! `cpt-cf-oagw-feature-upstream-management` (2.2),
//! `cpt-cf-oagw-feature-route-management` (2.3), and the plugin-management
//! entry (2.4).

pub mod plugin;
pub mod route;
pub mod upstream;
