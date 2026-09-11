//! Domain layer: entities, value objects and alias derivation. No I/O.

pub mod alias;
pub mod plugin;
pub mod route;
pub mod upstream;

/// The default value of the `enabled` flag on every entity.
#[must_use]
pub const fn default_enabled() -> bool {
    true
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod alias_tests;
