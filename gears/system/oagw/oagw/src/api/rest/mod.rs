//! REST surface of the `oagw` gear.
//!
//! `error` implements the error-response contract; `route_shell` carries the
//! management and proxy route shell registered under `/oagw/v1`; `proxy_handler`
//! is the proxy handler the shell routes to, the caller of the data-plane
//! pipeline of `cpt-cf-oagw-feature-proxy-pipeline`.

pub mod error;
pub mod proxy_handler;
pub mod route_shell;
