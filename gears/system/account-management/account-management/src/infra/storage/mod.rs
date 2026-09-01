//! Storage layer — `SeaORM` entities and repository implementations for
//! Account Management tables.
//!
//! Exposes `entity` (column-for-column entities), `migrations`,
//! `repo_impl` (the SeaORM-backed `TenantRepo` implementation), and
//! `integrity` (the Rust-side hierarchy-integrity classifier pipeline
//! — snapshot loader, single-flight gate, and the eight pure-Rust
//! classifiers that produce the [`crate::domain::tenant::integrity::IntegrityReport`]).

pub mod entity;
pub mod integrity;
pub mod migrations;
pub mod repo_impl;
