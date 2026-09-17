//! API layer — REST transport for the OAGW gear.
//!
//! DDD-Light layering: `api/rest` → `domain` → `infra` (DoD
//! `cpt-cf-oagw-dod-gear-foundation-skeleton`; feature
//! `cpt-cf-oagw-feature-gear-foundation`).  Handlers, route
//! registration, DTOs, and the RFC 9457 error mapping live under `rest`;
//! they are added by the Control-Plane Management API and Data-Plane Proxy
//! features (phases p3/p5).  This module currently carries the empty
//! registration surface (plane classification plus the shared-state
//! Extension) wired by [`crate::gear::OagwGear`].

pub mod rest;
