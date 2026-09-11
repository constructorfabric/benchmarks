// Updated: 2026-09-01 by Constructor Tech
//! Storage for the Control Plane.
//!
//! DESIGN pins an in-memory store: a gear restart loses every upstream, route
//! and custom plugin, which is the accepted durability posture for this
//! component. [`memory::Stores`] is the concrete bundle; [`memory::repos`]
//! hands out the trait objects the domain services take.

pub mod memory;
