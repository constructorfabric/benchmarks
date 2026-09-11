// Updated: 2026-09-01 by Constructor Tech
//! The Data Plane.
//!
//! [`service::ProxyService`] is the front door: it resolves the alias through
//! the caller's tenant chain, matches a route, merges the effective
//! configuration and hands the result to [`engine`], which owns the wire.
//! [`headers`], [`ratelimit`], [`circuit`] and [`ssrf`] are the pieces the
//! engine composes.

pub mod alias;
pub mod circuit;
pub mod engine;
pub mod error;
pub mod headers;
pub mod ratelimit;
pub mod service;
pub mod ssrf;
pub mod upgrade;
