//! OAGW REST API layer (DESIGN §3.3 "API Contracts").
//!
//! Transport-side DTOs and handlers for the management CRUD surface
//! (`/oagw/v1/upstreams|routes|plugins`) and the data-plane proxy endpoint
//! (`/oagw/v1/proxy/{alias}[/{path}]`). The layer is thin: it parses/extracts
//! requests, applies transport-level validation (body size, CORS preflight),
//! delegates to the control-plane or data-plane service, and renders
//! responses — including the RFC 9457 problem envelope with the OAGW GTS
//! error `type` identifiers and `X-OAGW-Error-Source` (ADR-0007).

pub mod rest;
