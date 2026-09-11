//! REST transport of the OAGW gear.
//!
//! Feature 1 (Gear Foundation) populates the route mount, the DTOs that carry
//! the request context into the error mapping, the single domain-error to
//! `application/problem+json` mapping layer and the `X-OAGW-Error-Source`
//! response-header layer. Entry 2.2 adds [`handlers`], the ten management
//! endpoints, and [`openapi`], the operations and schemas it registers in the
//! host registry. Entry 2.4 adds [`proxy`], the data-plane transport the proxy
//! engine serves through.

pub mod dto;
pub mod error;
pub mod handlers;
pub mod openapi;
pub mod proxy;
pub mod routes;
