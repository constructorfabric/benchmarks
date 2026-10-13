//! Attachments (`cpt-cf-mini-chat-feature-attachments`): upload validation and
//! classification, provider file storage and indexing, thumbnails, get, soft
//! delete, the cleanup payload and the ports of the infrastructure layer.
//!
//! The domain depends on the ports of [`ports`] only: the attachment store, the
//! provider storage client and directory, the background runtime and the
//! metrics sink. It never sees `SeaORM`, the OAGW SDK or HTTP types.

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-quality-gates:p1

pub mod classify;
pub mod detail;
pub mod model;
pub mod payload;
pub mod ports;
pub mod service;
pub mod source;
pub mod thumbnail;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "classify_tests.rs"]
mod classify_tests;

#[cfg(test)]
#[path = "detail_tests.rs"]
mod detail_tests;

#[cfg(test)]
#[path = "payload_tests.rs"]
mod payload_tests;

#[cfg(test)]
#[path = "source_tests.rs"]
mod source_tests;

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;

#[cfg(test)]
#[path = "service_timing_tests.rs"]
mod service_timing_tests;
