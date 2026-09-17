//! REST API surface for the OAGW gear (`cpt-cf-oagw-feature-control-plane`).
//!
//! The management surface (`/oagw/v1/upstreams`, `/oagw/v1/routes`,
//! `/oagw/v1/plugins`) and the proxying surface
//! (`{METHOD} /oagw/v1/proxy/{alias}/{*rest}`) live under [`rest`], all
//! gear-relative with no `/api` segment (ADR: `cpt-cf-oagw-constraint-no-api-segment`).

pub mod rest;
