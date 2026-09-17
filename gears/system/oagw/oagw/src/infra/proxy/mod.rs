//! Data-plane transport: body framing, header rules, dialling and bridging.

pub mod body;
pub mod egress;
pub mod http;
pub mod service;
pub mod ws;

pub use service::{ProxyRequest, ProxyResponse, ProxyService};
