// Created: 2026-08-31 by Constructor Tech
//! Infrastructure layer: integrations with the world outside the domain
//! (ADR-0002 "Built-in Plugins").
//!
//! [`plugin`] holds the plugin contracts of ADR-0002 and the built-in plugins
//! that ship with the gateway. [`metrics`] holds the OpenTelemetry instruments
//! of the data plane (DESIGN §4.2), emitted against the meter provider the
//! host installs. Everything here is behind a domain-facing interface only:
//! the proxy pipeline sees the traits, never a concrete plugin.

pub mod metrics;
pub mod plugin;
