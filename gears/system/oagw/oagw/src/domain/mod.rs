//! Domain layer: model, errors, policy and the plugin contracts.

pub mod alias;
pub mod cors;
pub mod dto;
pub mod error;
pub mod gts_helpers;
pub mod layering;
pub mod matching;
pub mod plugin;
pub mod ratelimit;
pub mod repo;
pub mod services;

#[cfg(test)]
mod dto_tests;
#[cfg(test)]
mod alias_tests;
#[cfg(test)]
mod matching_tests;
#[cfg(test)]
mod layering_tests;
#[cfg(test)]
mod ratelimit_tests;
