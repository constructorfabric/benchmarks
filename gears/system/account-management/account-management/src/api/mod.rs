//! Public API surface for AM (REST). PEP is enforced by domain services — handlers pass `SecurityContext` through but never construct `AccessRequest`.

pub mod rest;
