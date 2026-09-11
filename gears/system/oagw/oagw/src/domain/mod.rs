//! Domain model, validation, alias rules and the configuration store.

pub mod alias;
pub mod error;
pub mod model;
pub mod store;

pub use error::OagwError;
pub use store::Store;
