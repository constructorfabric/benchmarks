//! Infrastructure adapters of the OAGW gear: the in-memory store, the
//! credential store resolver, the rate limiter, the CORS handler, the plugin
//! framework and the upgrade tunnel the data plane drives.

pub mod cors;
pub mod http_client;
pub mod plugins;
pub mod ratelimit;
pub mod secrets;
pub mod store;
pub mod upgrade;

#[cfg(test)]
pub(crate) mod test_support;
