//! `Alias`, `Hostname`, and `EndpointHost` value objects.
//!
//! Realizes `cpt-cf-oagw-algo-alias-normalize`: normalization and validation
//! live here once, so write-time storage and proxy-time resolution cannot
//! disagree about what an alias looks like. Alias derivation (single
//! hostname, longest common registrable suffix, rejection of a bare public
//! suffix) and alias immutability across updates are delivered by the
//! control-plane-config feature, which calls these constructors on every
//! value it stores or resolves.

use std::fmt;

use serde::{Deserialize, Serialize};

/// RFC 1123: maximum total length of a hostname (no trailing dot).
const MAX_HOSTNAME_LEN: usize = 253;
/// RFC 1123: maximum length of a single hostname label.
const MAX_LABEL_LEN: usize = 63;

/// Why a normalized alias or host string was rejected.
///
/// The offending input is deliberately not carried: `detail` never contains
/// configuration values, and an alias is an operator-supplied string.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    /// The value was empty once surrounding whitespace and trailing dots
    /// were stripped.
    #[error("alias or host is empty after trimming")]
    Empty,
    /// The value carried a non-ASCII byte; it is never transliterated.
    #[error("alias or host carries a non-ASCII byte")]
    NonAscii,
    /// A label violated RFC 1123: empty, leading or trailing hyphen, or a
    /// character outside `[a-z0-9-]`.
    #[error("alias or host carries an RFC 1123-invalid label")]
    InvalidLabel,
    /// The `:port` suffix was not an integer from 1 to 65535.
    #[error("port must be an integer from 1 to 65535")]
    InvalidPort,
    /// The value, or one of its labels, exceeded the RFC 1123 length limit.
    #[error("alias or host exceeds the RFC 1123 length limit")]
    TooLong,
}

/// Normalizes an input string: trims surrounding whitespace, rejects
/// non-ASCII bytes, and lowercases to ASCII.
fn normalize(input: &str) -> Result<String, AliasError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-trim
    let trimmed = input.trim();
    // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-trim

    // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-empty-if
    if trimmed.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-empty-return
        return Err(AliasError::Empty);
        // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-empty-return
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-empty-if

    // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-lower
    if !trimmed.is_ascii() {
        return Err(AliasError::NonAscii);
    }
    let lowered = trimmed.to_ascii_lowercase();
    // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-lower

    Ok(lowered)
}

/// Strips all trailing dots: FQDN notation is tolerated on input and never
/// stored. Applied to the host part so a `:port` suffix cannot hide the dots
/// the caller wrote before it.
fn strip_trailing_dots(host: &str) -> Result<&str, AliasError> {
    let stripped = host.trim_end_matches('.');
    if stripped.is_empty() {
        return Err(AliasError::Empty);
    }
    Ok(stripped)
}

/// Validates RFC 1123 hostname syntax on an already-normalized string.
fn validate_rfc1123(host: &str) -> Result<(), AliasError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-rfc1123
    if host.is_empty() {
        return Err(AliasError::Empty);
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(AliasError::TooLong);
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(AliasError::InvalidLabel);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(AliasError::TooLong);
        }
        let hyphen_edged = label.starts_with('-') || label.ends_with('-');
        let in_label_set = label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if hyphen_edged || !in_label_set {
            return Err(AliasError::InvalidLabel);
        }
    }
    Ok(())
    // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-rfc1123
}

/// Splits a normalized `host:port` value, validating the port as an integer
/// from 1 to 65535. The port participates in alias identity, so it is kept in
/// the value rather than folded away.
fn split_port(normalized: &str) -> Result<(&str, Option<u16>), AliasError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-port-if
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-port-keep
    let Some((host, port_raw)) = normalized.rsplit_once(':') else {
        return Ok((normalized, None));
    };
    if host.is_empty() || port_raw.is_empty() {
        return Err(AliasError::InvalidPort);
    }
    let port: u16 = port_raw.parse().map_err(|_| AliasError::InvalidPort)?;
    if port == 0 {
        return Err(AliasError::InvalidPort);
    }
    Ok((host, Some(port)))
    // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-port-keep
    // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-port-if
}

/// A normalized host name with no port: RFC 1123 syntax, ASCII lowercase,
/// trailing dots stripped.
///
/// Construction goes through [`Hostname::parse`] or the `TryFrom`
/// conversions; there is no way to build one from an unnormalized string.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hostname(String);

impl Hostname {
    /// Parses and normalizes a host name.
    ///
    /// # Errors
    ///
    /// Returns [`AliasError`] when the input is empty, carries a non-ASCII
    /// byte, or violates RFC 1123.
    pub fn parse(input: &str) -> Result<Self, AliasError> {
        let normalized = normalize(input)?;
        let host = strip_trailing_dots(&normalized)?;
        validate_rfc1123(host)?;
        // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-return
        Ok(Self(host.to_owned()))
        // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-return
    }

    /// The normalized host name, with no port and no trailing dot.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for Hostname {
    type Error = AliasError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for Hostname {
    type Error = AliasError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Hostname> for String {
    fn from(value: Hostname) -> Self {
        value.0
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A normalized routing alias: a [`Hostname`] plus an optional port.
///
/// The port participates in identity, so `api.openai.com` and
/// `api.openai.com:8443` are distinct aliases. Construction goes through
/// [`Alias::parse`] or the `TryFrom` conversions, and the value round-trips
/// through its normalized `Display` form.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Alias {
    host: Hostname,
    port: Option<u16>,
}

impl Alias {
    /// Parses and normalizes an alias, optionally carrying a `:port` suffix.
    ///
    /// # Errors
    ///
    /// Returns [`AliasError`] when the input is empty, carries a non-ASCII
    /// byte, violates RFC 1123, or carries an out-of-range port.
    pub fn parse(input: &str) -> Result<Self, AliasError> {
        let normalized = normalize(input)?;
        let (host_part, port) = split_port(&normalized)?;
        let host = strip_trailing_dots(host_part)?;
        validate_rfc1123(host)?;
        // @cpt-begin:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-return
        Ok(Self {
            host: Hostname(host.to_owned()),
            port,
        })
        // @cpt-end:cpt-cf-oagw-algo-alias-normalize:p1:inst-alias-return
    }

    /// The normalized host part of the alias, with no port.
    #[must_use]
    pub fn host(&self) -> &Hostname {
        &self.host
    }

    /// The port part of the alias, when the input carried one.
    #[must_use]
    pub const fn port(&self) -> Option<u16> {
        self.port
    }
}

impl TryFrom<&str> for Alias {
    type Error = AliasError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for Alias {
    type Error = AliasError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Alias> for String {
    fn from(value: Alias) -> Self {
        value.to_string()
    }
}

impl fmt::Display for Alias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            None => f.write_str(self.host.as_str()),
            Some(port) => write!(f, "{}:{port}", self.host.as_str()),
        }
    }
}

/// The `host` value of a configured endpoint: an RFC 1123 host name or an IP
/// literal (IPv4 or IPv6).
///
/// The shipped upstream schema admits all three forms for
/// `server.endpoints[].host` while [`Hostname`] admits only RFC 1123 names,
/// so endpoints carry their own value object rather than weakening
/// [`Hostname`]. The port is a separate endpoint property and is never
/// carried here.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EndpointHost(String);

impl EndpointHost {
    /// Parses and normalizes an endpoint host, accepting an RFC 1123 name or
    /// an IPv4/IPv6 literal.
    ///
    /// # Errors
    ///
    /// Returns [`AliasError`] when the input is empty, carries a non-ASCII
    /// byte, violates RFC 1123, and is not an IP literal.
    pub fn parse(input: &str) -> Result<Self, AliasError> {
        let normalized = normalize(input)?;
        let host = strip_trailing_dots(&normalized)?;
        if validate_rfc1123(host).is_ok() {
            return Ok(Self(host.to_owned()));
        }
        let literal = host
            .parse::<std::net::IpAddr>()
            .map_err(|_| AliasError::InvalidLabel)?;
        Ok(Self(literal.to_string()))
    }

    /// The normalized endpoint host.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for EndpointHost {
    type Error = AliasError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for EndpointHost {
    type Error = AliasError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<EndpointHost> for String {
    fn from(value: EndpointHost) -> Self {
        value.0
    }
}

impl fmt::Display for EndpointHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
