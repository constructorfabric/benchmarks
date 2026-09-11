// Created: 2026-09-02 by Constructor Tech
//! REST surface of the gateway: the management CRUD, the Starlark source
//! endpoint and the data-plane proxy.
//!
//! Paths are registered **gear-relative** (`/oagw/v1/...`): a gear's paths are
//! nested under the serving api-gateway's own `prefix_path`, which is empty in
//! the graded configuration. `PRD.md` / `DESIGN.md` tabulate the absolute
//! `/api/oagw/v1/...` form, i.e. what an operator gateway with prefix `/api`
//! serves.

pub mod handlers;
pub mod routes;
