#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! # `toolkit-sdk` - SDK utilities for toolkit-based applications
//!
//! This crate provides utilities for building SDKs on top of toolkit, including:
//!
//! - **Security context scoping** (`secured` gear) - Zero-allocation wrapper for binding
//!   `SecurityContext` to clients
//! - **Type-safe `OData` queries** (`odata` gear) - Fluent query builder with compile-time
//!   field validation
//! - **Cursor-based pagination** (`pager` gear) - Stream API for paginated results
//!
//! ## Example
//!
//! ```rust,ignore
//! use toolkit_sdk::secured::WithSecurityContext;
//! use toolkit_sdk::odata::QueryBuilder;
//! use toolkit_security::SecurityContext;
//!
//! let client = MyClient::new();
//! let ctx = SecurityContext::root();
//!
//! // Bind security context to client
//! let secured = client.security_ctx(&ctx);
//!
//! // Build type-safe query
//! let query = QueryBuilder::<UserSchema>::new()
//!     .filter(NAME.contains("john"))
//!     .page_size(50)
//!     .build();
//! ```

pub mod odata;
pub mod pager;
pub mod secured;

// Re-export commonly used types for convenience
pub use pager::PagerError;
pub use secured::{Secured, WithSecurityContext};

// Re-export proc-macros (feature-gated)
#[cfg(feature = "derive")]
pub use toolkit_odata_macros::ODataSchema;
