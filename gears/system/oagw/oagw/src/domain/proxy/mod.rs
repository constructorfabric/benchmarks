//! Plain-HTTP proxy data plane (`cpt-cf-oagw-feature-http-proxy`), extended
//! by the streaming feature (`cpt-cf-oagw-feature-streaming-proxy`) with
//! pass-through SSE streaming ([`stream`]) and WebSocket upgrade proxying
//! ([`websocket`]).
//!
//! Consumes [`super::resolve::resolve_proxy_target`]'s [`super::ResolvedPlan`]
//! and executes the proxy request lifecycle: CORS preflight short-circuit
//! ([`cors`]), guard evaluation ([`guard`]), body validation ([`body`]),
//! the plugin/rate-limit hook seam ([`plugin_seam`]), header transformation
//! ([`headers`]), endpoint selection ([`endpoint`]), and upstream invocation
//! ([`upstream`]). [`crate::api::rest::handlers::proxy`] is the transport-layer
//! caller that ties these stages together, including the streamed and
//! upgraded paths.

pub mod body;
pub mod cors;
pub mod endpoint;
pub mod guard;
pub mod headers;
pub mod plugin_seam;
pub mod stream;
pub mod upstream;
pub mod websocket;
