//! REST surface for AM. Layout mirrors `resource-group`: dto (wire shapes), handlers (axum), routes (`OperationBuilder`). Endpoint families are enumerated in `docs/account-management-v1.yaml`.

pub mod dto;
pub mod handlers;
pub mod routes;
