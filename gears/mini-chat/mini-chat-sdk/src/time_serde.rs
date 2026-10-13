//! Serde helpers that put `time::OffsetDateTime` on the wire as an RFC 3339 string.
//!
//! Use as `#[serde(with = "mini_chat_sdk::time_serde")]`.

use serde::{Deserialize, Deserializer, Serializer, de, ser};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Serializes the timestamp as an RFC 3339 string.
///
/// # Errors
/// Fails when the timestamp cannot be formatted (years outside the RFC 3339 range).
pub fn serialize<S: Serializer>(value: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error> {
    let text = value
        .format(&Rfc3339)
        .map_err(|_| ser::Error::custom("timestamp is not representable as RFC 3339"))?;
    serializer.serialize_str(&text)
}

/// Deserializes an RFC 3339 string into a timestamp.
///
/// # Errors
/// Fails when the input is not a valid RFC 3339 timestamp.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<OffsetDateTime, D::Error> {
    let text = String::deserialize(deserializer)?;
    OffsetDateTime::parse(&text, &Rfc3339)
        .map_err(|_| de::Error::custom(format!("invalid RFC 3339 timestamp: {text}")))
}
