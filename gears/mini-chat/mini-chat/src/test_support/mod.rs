//! Test doubles shared by unit tests and integration tests (always compiled,
//! hidden from docs).

pub mod fake_oagw;

pub use fake_oagw::{FakeOagw, MultipartField, RecordedRequest, SseStep};
