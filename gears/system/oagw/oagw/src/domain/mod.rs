//! Business logic for the `oagw` gear.
//!
//! This layer holds no transport or storage types: `model`, `alias`, `matcher`
//! and `error` are pure, `repo` declares the persistence contracts and
//! `services` hosts the control-plane and data-plane orchestration.

pub mod alias;
pub mod error;
pub mod matcher;
pub mod merge;
pub mod model;
pub mod plugin;
pub mod repo;
pub mod services;
