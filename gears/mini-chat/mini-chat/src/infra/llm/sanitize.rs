//! Provider message sanitizer (DESIGN 3.3 "Provider identifier non-exposure
//! invariant"). The rule is a client-boundary invariant owned by the domain;
//! the adapters re-export it so provider text is cleaned where it enters the
//! gear.

pub use crate::domain::sanitize::sanitize_provider_message;
