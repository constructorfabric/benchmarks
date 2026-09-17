//! The data plane: everything the proxy needs to relay an exchange.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`pipeline`] | stage order, plugin execution, the request/response relay |
//! | [`resolve`] | alias, route and tenant-hierarchy resolution |
//! | [`transport`] | the hyper client, dial policy and body idle timeouts |
//! | [`upgrades`] | `Upgrade` handshake relay and socket splicing |
//! | [`guards`] | core request guards + the outbound SSRF policy |
//! | [`rate_limit`] | token-bucket / sliding-window counters (ADR 0003) |
//! | [`circuit`] | per-upstream circuit breakers |
//! | [`headers`] | hop-by-hop hygiene and operator header rules |
//! | [`secrets`] | credstore-backed secret resolution for plugins |
//! | [`runtime`] | the `PluginRuntime` (secrets, token cache, transport) |
//! | [`metrics`] | data-plane counters and labels |
//! | [`base64`] | `Basic` credentials (no new dependency) |

pub mod base64;
pub mod circuit;
pub mod guards;
pub mod headers;
pub mod metrics;
pub mod pipeline;
pub mod rate_limit;
pub mod resolve;
pub mod runtime;
pub mod secrets;
pub mod transport;
pub mod upgrades;

pub use circuit::{COOLDOWN, CircuitBreakers, FAILURE_THRESHOLD};
pub use guards::{BodyPolicy, SsrfPolicy};
pub use metrics::DpMetrics;
pub use pipeline::{DataPlaneService, PipelineFailure};
pub use rate_limit::RateLimiter;
pub use resolve::{MatchedRoute, MergedUpstream, RouteMatchResult};
pub use runtime::PluginRuntime;
pub use secrets::{CredStoreSecretSource, InMemorySecretSource, ResolvedSecret, SecretSource};
pub use transport::{ProxyClient, ProxyTransport, TimedBody, TransportError};
