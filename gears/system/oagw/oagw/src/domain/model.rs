//! Domain entities of the OAGW gear.
//!
//! DECOMPOSITION entry 2.2 (upstream and route management) models the entities
//! declared by Feature 1: [`Upstream`], [`Route`] and their parts, with the
//! field names, enums and recorded defaults of
//! `docs/schemas/upstream.v1.schema.json` and `docs/schemas/route.v1.schema.json`.
//!
//! These are the *stored* representations: every optional field holds the value
//! the validator resolved, so the store holds exactly what the data plane (entry
//! 2.4) reads. The request shapes and their validation live in
//! [`crate::domain::validation`], the repository contracts in
//! [`crate::domain::repo`], and the service flows in [`crate::domain::service`].
//!
//! No type below carries secret material (`cpt-cf-oagw-principle-cred-isolation`):
//! the only credential surface is [`AuthConfig::config`], whose string values the
//! upstream validator constrains to `cred://` references.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// `protocol` value of an HTTP upstream.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// `protocol` value of a gRPC upstream.
///
/// Declared and stored, but this deployment serves no gRPC traffic (DESIGN
/// Phase 3): entry 2.6 owns the proxy path that would consume it.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// @cpt-begin:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-11
/// Wall-clock instant, stored as nanoseconds since the Unix epoch.
///
/// The store stamps it on insert and refreshes it on replace, and the list
/// contract orders on it (`$orderby=created_at desc`), so the integer form is
/// the ordering key. It renders as an RFC 3339 UTC timestamp on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    /// Nanoseconds since the Unix epoch.
    nanos: u128,
}

impl Timestamp {
    /// Current wall-clock instant.
    #[must_use]
    pub fn now() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self { nanos }
    }

    /// Instant at `nanos` since the Unix epoch.
    #[must_use]
    pub const fn from_nanos(nanos: u128) -> Self {
        Self { nanos }
    }

    /// Nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn as_nanos(&self) -> u128 {
        self.nanos
    }

    /// Render the instant as an RFC 3339 UTC timestamp with millisecond
    /// precision.
    #[must_use]
    pub fn to_rfc3339(self) -> String {
        let millis = u64::try_from(self.nanos / 1_000_000).unwrap_or(0);
        let (date, time) = render_utc(millis);
        format!("{date}T{time}Z")
    }

    /// Parse an RFC 3339 UTC timestamp with millisecond precision.
    ///
    /// Accepts exactly the form [`Self::to_rfc3339`] emits plus an optional
    /// fractional part, so a value round-trips; anything else is rejected.
    pub fn parse_rfc3339(raw: &str) -> Option<Self> {
        let (date, rest) = raw.split_once('T')?;
        if !rest.ends_with('Z') {
            return None;
        }
        let clock = &rest[..rest.len() - 1];
        let (time, fraction) = match clock.split_once('.') {
            Some((time, fraction)) => (time, fraction),
            None => (clock, ""),
        };
        let (year, month, day) = parse_date(date)?;
        let mut parts = time.split(':');
        let hour: u32 = parts.next()?.parse().ok()?;
        let minute: u32 = parts.next()?.parse().ok()?;
        let second: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        if !fraction.is_empty() && fraction.len() > 9 {
            return None;
        }
        let millis: u64 = match fraction.len() {
            0 => 0,
            1..=3 => format!("{fraction:0<3}").parse().ok()?,
            _ => fraction[..3].parse().ok()?,
        };
        let days = days_from_civil(i64::from(year), month, day);
        let seconds = u64::try_from(days).ok()? * 86_400
            + u64::from(hour) * 3_600
            + u64::from(minute) * 60
            + u64::from(second);
        let nanos = u128::from(seconds) * 1_000_000_000 + u128::from(millis) * 1_000_000;
        Some(Self { nanos })
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_rfc3339(&raw).ok_or_else(|| D::Error::custom("invalid timestamp"))
    }
}

/// Milliseconds of the day and the day count of a millisecond epoch value.
fn render_utc(millis: u64) -> (String, String) {
    let days = i64::try_from(millis / 86_400_000).unwrap_or(0);
    let rem = millis % 86_400_000;
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3_600_000;
    let minute = (rem / 60_000) % 60;
    let second = (rem / 1_000) % 60;
    let fraction = rem % 1_000;
    (
        format!("{year:04}-{month:02}-{day:02}"),
        format!("{hour:02}:{minute:02}:{second:02}.{fraction:03}"),
    )
}

/// Split `YYYY-MM-DD` into its numeric parts.
fn parse_date(raw: &str) -> Option<(u32, u32, u32)> {
    let mut parts = raw.split('-');
    let year: u32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// Days since the Unix epoch for a civil date (Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let d = i64::from(day);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for a day count since the Unix epoch (Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}
// @cpt-end:cpt-cf-oagw-algo-upstream-validate:p1:inst-uval-11

/// Transport scheme of one upstream endpoint (`server.endpoints[].scheme`).
///
/// The schema enumerates five values and defaults to `https`
/// (DECOMPOSITION assumption 2 records that `http` is a legal scheme here, gated
/// by the `allow_http_upstream` configuration key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// Plain HTTP.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebSocket over plain TCP.
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl Scheme {
    /// The scheme name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }

    /// The port the schema defaults to when the endpoint omits `port`.
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Parse a scheme name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "wss" => Some(Self::Wss),
            "wt" => Some(Self::Wt),
            "grpc" => Some(Self::Grpc),
            _ => None,
        }
    }
}

/// Derive the string round-trip of an enum that exposes `as_str` and `parse`.
///
/// The wire form is the schema name, so a stored record re-reads as the same
/// value and an unknown name is a deserialization failure.
macro_rules! string_enum {
    ($ty:ty, $label:literal) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                <$ty>::parse(&raw)
                    .ok_or_else(|| D::Error::custom(concat!("unknown ", $label)))
            }
        }
    };
}

string_enum!(Scheme, "endpoint scheme");
string_enum!(Protocol, "protocol identifier");
string_enum!(MatchType, "match kind");
string_enum!(SuffixMode, "path_suffix_mode");
string_enum!(Sharing, "sharing scope");
string_enum!(RateWindow, "rate window");
string_enum!(RateAlgorithm, "rate algorithm");
string_enum!(RateScope, "rate scope");
string_enum!(RateStrategy, "rate strategy");
string_enum!(HeaderPassthrough, "passthrough mode");

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `private`, the schema default of every `sharing` member.
impl Default for Sharing {
    fn default() -> Self {
        Self::Private
    }
}

/// `second`, the schema default of `sustained.window`.
impl Default for RateWindow {
    fn default() -> Self {
        Self::Second
    }
}

/// `token_bucket`, the schema default of `algorithm`.
impl Default for RateAlgorithm {
    fn default() -> Self {
        Self::TokenBucket
    }
}

/// `tenant`, the schema default of `scope`.
impl Default for RateScope {
    fn default() -> Self {
        Self::Tenant
    }
}

/// `reject`, the schema default of `strategy`.
impl Default for RateStrategy {
    fn default() -> Self {
        Self::Reject
    }
}

/// `none`, the schema default of `passthrough`.
impl Default for HeaderPassthrough {
    fn default() -> Self {
        Self::None
    }
}

/// `append`, the schema default of `path_suffix_mode`.
impl Default for SuffixMode {
    fn default() -> Self {
        Self::Append
    }
}

/// Upstream protocol (`protocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// HTTP upstream.
    Http,
    /// gRPC upstream.
    Grpc,
}

impl Protocol {
    /// The GTS identifier as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }

    /// Parse a protocol GTS identifier.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw == PROTOCOL_HTTP {
            Some(Self::Http)
        } else if raw == PROTOCOL_GRPC {
            Some(Self::Grpc)
        } else {
            None
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Route match kind, derived from the `match` member (`route.match_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchType {
    /// `match.http` was present.
    Http,
    /// `match.grpc` was present.
    Grpc,
}

impl MatchType {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Grpc => "grpc",
        }
    }

    /// Parse a match-kind name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "http" => Some(Self::Http),
            "grpc" => Some(Self::Grpc),
            _ => None,
        }
    }
}

impl fmt::Display for MatchType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `match.http.path_suffix_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuffixMode {
    /// Strip the matched prefix (`disabled`).
    Disabled,
    /// Append the suffix to the upstream path (`append`, the default).
    Append,
}

impl SuffixMode {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Append => "append",
        }
    }

    /// Parse a mode name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "disabled" => Some(Self::Disabled),
            "append" => Some(Self::Append),
            _ => None,
        }
    }
}

impl fmt::Display for SuffixMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// HTTP method a route may match (`match.http.methods[]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH`
    Patch,
}

impl HttpMethod {
    /// The method name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }

    /// Parse a method name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// HTTP method a CORS configuration may name (`*.allowed_methods[]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CorsMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// `HEAD`
    Head,
    /// `OPTIONS`
    Options,
}

impl CorsMethod {
    /// The method name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }

    /// Parse a CORS method name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "PATCH" => Some(Self::Patch),
            "DELETE" => Some(Self::Delete),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            _ => None,
        }
    }
}

/// Scope a shared configuration block applies to (`*.sharing`).
///
/// The gateway consumer decides; the management surface only records the value
/// and validates that the declared sharing is compatible with the ancestor chain
/// (`cpt-cf-oagw-algo-sharing-mode-validate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sharing {
    /// `private` — visible to, and overridable by, the owning tenant only.
    Private,
    /// `inherit` — descendants inherit the value and may override it.
    Inherit,
    /// `enforce` — descendants inherit the value and may not override it.
    Enforce,
}

impl Sharing {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }

    /// Parse a sharing name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "private" => Some(Self::Private),
            "inherit" => Some(Self::Inherit),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }
}

impl fmt::Display for Sharing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Window unit of a sustained rate limit (`*.rate_limit.sustained.window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateWindow {
    /// `second`
    Second,
    /// `minute`
    Minute,
    /// `hour`
    Hour,
    /// `day`
    Day,
}

impl RateWindow {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

    /// Window length in seconds.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }

    /// Parse a window name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "second" => Some(Self::Second),
            "minute" => Some(Self::Minute),
            "hour" => Some(Self::Hour),
            "day" => Some(Self::Day),
            _ => None,
        }
    }
}

impl fmt::Display for RateWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sustained-rate algorithm (`*.rate_limit.algorithm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateAlgorithm {
    /// `token_bucket`
    TokenBucket,
    /// `sliding_window`
    SlidingWindow,
}

impl RateAlgorithm {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TokenBucket => "token_bucket",
            Self::SlidingWindow => "sliding_window",
        }
    }

    /// Parse an algorithm name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "token_bucket" => Some(Self::TokenBucket),
            "sliding_window" => Some(Self::SlidingWindow),
            _ => None,
        }
    }
}

impl fmt::Display for RateAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Key a rate limiter counts by (`*.rate_limit.scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateScope {
    /// `global`
    Global,
    /// `tenant`
    Tenant,
    /// `user`
    User,
    /// `ip`
    Ip,
    /// `route`
    Route,
}

impl RateScope {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Tenant => "tenant",
            Self::User => "user",
            Self::Ip => "ip",
            Self::Route => "route",
        }
    }

    /// Parse a scope name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "global" => Some(Self::Global),
            "tenant" => Some(Self::Tenant),
            "user" => Some(Self::User),
            "ip" => Some(Self::Ip),
            "route" => Some(Self::Route),
            _ => None,
        }
    }
}

impl fmt::Display for RateScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a rejected request is discharged (`*.rate_limit.strategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateStrategy {
    /// `reject`
    Reject,
    /// `queue`
    Queue,
    /// `degrade`
    Degrade,
}

impl RateStrategy {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Queue => "queue",
            Self::Degrade => "degrade",
        }
    }

    /// Parse a strategy name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "reject" => Some(Self::Reject),
            "queue" => Some(Self::Queue),
            "degrade" => Some(Self::Degrade),
            _ => None,
        }
    }
}

impl fmt::Display for RateStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Header disposition (`*.headers.*.passthrough`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderPassthrough {
    /// `all`
    All,
    /// `allowlist`
    Allowlist,
    /// `none`
    None,
}

impl HeaderPassthrough {
    /// The name as it appears in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Allowlist => "allowlist",
            Self::None => "none",
        }
    }

    /// Parse a passthrough name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "all" => Some(Self::All),
            "allowlist" => Some(Self::Allowlist),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

impl fmt::Display for HeaderPassthrough {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One upstream endpoint (`server.endpoints[]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Endpoint scheme; defaults to `https` when the request omitted it.
    pub scheme: Scheme,
    /// Hostname, IPv4 or IPv6 literal.
    pub host: String,
    /// Port; defaults to the scheme's standard port when the request omitted it.
    pub port: u16,
}

/// Endpoint pool of an upstream (`server`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoints, in declaration order (at least one, per the schema).
    pub endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// The endpoint pool, i.e. the endpoints of the same protocol, scheme and
    /// port as the first endpoint (DESIGN "multi-endpoint pool rule").
    ///
    /// Entry 2.4 balances across the returned slice; endpoints outside the pool
    /// are stored but not dialed.
    #[must_use]
    pub fn pool(&self) -> Vec<Endpoint> {
        let Some(first) = self.endpoints.first() else {
            return Vec::new();
        };
        self.endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.protocol_kind() == first.protocol_kind() && endpoint.port == first.port
            })
            .cloned()
            .collect()
    }
}

impl Endpoint {
    /// The protocol a request to this endpoint would speak.
    ///
    /// `grpc` and the WebSocket schemes are pooled separately from HTTP, which
    /// is what the DESIGN pool rule keys on ("same protocol/scheme/port").
    #[must_use]
    pub fn protocol_kind(&self) -> &'static str {
        match self.scheme {
            Scheme::Grpc => "grpc",
            Scheme::Wss | Scheme::Wt => "websocket",
            Scheme::Http | Scheme::Https => "http",
        }
    }
}

/// Upstream authentication declaration (`auth`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier (`auth.type`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Sharing scope of the plugin configuration (`auth.sharing`).
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin configuration values (`auth.config`); only `cred://` references.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

/// Header manipulation declared on an upstream or a route (`headers`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Request-side manipulation (`headers.request`).
    #[serde(default)]
    pub request: RequestHeaders,
    /// Response-side manipulation (`headers.response`).
    #[serde(default)]
    pub response: ResponseHeaders,
}

/// Request-side header manipulation (`headers.request`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RequestHeaders {
    /// Overwrite on every request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Add without overwriting.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Remove by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Client header disposition.
    #[serde(default, skip_serializing_if = "is_default_passthrough")]
    pub passthrough: HeaderPassthrough,
    /// Names passed through when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

impl HeaderPassthrough {
    /// Whether the value equals the schema default.
    #[must_use]
    pub const fn is_default(self) -> bool {
        matches!(self, Self::None)
    }
}

/// Response-side header manipulation (`headers.response`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResponseHeaders {
    /// Overwrite on every response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Add without overwriting.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Remove by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Sustained rate of a rate limit (`*.rate_limit.sustained`, required).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Requests allowed per [`Self::window`].
    pub rate: u64,
    /// Window unit.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst allowance of a rate limit (`*.rate_limit.burst`).
///
/// The schema makes `capacity` optional; the recorded default is the sustained
/// rate, which the data plane resolves and this feature stores verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstRate {
    /// Bucket capacity; `None` when the request omitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

/// Rate limit declared on an upstream or a route (`rate_limit`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing scope of the declaration.
    #[serde(default)]
    pub sharing: Sharing,
    /// Sustained-rate algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate (required by the schema).
    pub sustained: SustainedRate,
    /// Burst allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstRate>,
    /// Counter key.
    #[serde(default)]
    pub scope: RateScope,
    /// Disposition of a rejected request.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Weight of a single request.
    #[serde(default, skip_serializing_if = "is_default_cost")]
    pub cost: u64,
}

/// `true` when the value equals `1` (the schema default of `cost`).
const fn is_default_cost(value: &u64) -> bool {
    *value == 1
}

/// `true` when the value equals `none` (the schema default of `passthrough`).
const fn is_default_passthrough(value: &HeaderPassthrough) -> bool {
    matches!(value, HeaderPassthrough::None)
}

impl RateLimitConfig {
    /// The effective burst capacity: the declared one, or the sustained rate
    /// when `burst` or `burst.capacity` was omitted.
    #[must_use]
    pub fn effective_capacity(&self) -> u64 {
        // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-02
        // The capacity is the declared `burst.capacity`, and the sustained rate
        // when the field was omitted.
        self.burst
            .and_then(|burst| burst.capacity)
            .unwrap_or(self.sustained.rate)
        // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-02
    }
}

/// CORS declaration on an upstream or a route (`cors`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing scope of the declaration.
    #[serde(default)]
    pub sharing: Sharing,
    /// Whether CORS handling is enabled (required by the schema).
    pub enabled: bool,
    /// Origins allowed to make cross-origin requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Methods allowed for preflight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials may be sent.
    #[serde(default)]
    pub allow_credentials: bool,
}

/// One plugin binding of a pipeline (`plugins.items[]`).
///
/// The API accepts a plugin reference string; the store keeps the canonical
/// string in `plugin_ref` and, only for a UUID-backed custom plugin, the parsed
/// UUID in `plugin_uuid` (DESIGN "Plugin Identification Model").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBinding {
    /// Pipeline position, contiguous from `0`.
    #[serde(default)]
    pub position: u32,
    /// Canonical plugin identifier string; always stored.
    #[serde(rename = "plugin_ref")]
    pub reference: String,
    /// Parsed UUID when the reference is UUID-backed; `None` for named plugins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// Declared configuration of the binding, stored verbatim and never
    /// validated against the referenced plugin's `config_schema`
    /// (`cpt-cf-oagw-algo-plugin-binding-validate`, `inst-pbnd-10`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl PluginBinding {
    /// Whether the reference is UUID-backed, i.e. a custom plugin.
    #[must_use]
    pub fn is_custom(&self) -> bool {
        self.plugin_uuid.is_some()
    }
}

/// Plugin pipeline declared on an upstream or a route (`plugins`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Sharing scope of the declaration.
    #[serde(default)]
    pub sharing: Sharing,
    /// Bindings, ordered by [`PluginBinding::position`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

// @cpt-begin:cpt-cf-oagw-dod-upstream-model:p1:inst-full
/// An upstream: a named, validated set of endpoints the gateway can proxy to.
///
/// `id`, `tenant_id` and `alias` are immutable (the replace flow rejects a
/// change of any of them); every other member is replaced wholesale by
/// `cpt-cf-oagw-flow-upstream-replace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Immutable identity.
    pub id: Uuid,
    /// Owning tenant; immutable and the key of every store lookup.
    pub tenant_id: Uuid,
    /// Whether the upstream serves traffic; defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Immutable tenant-unique name (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`).
    pub alias: String,
    /// Free-form labels (`^[a-z0-9_-]+$`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol GTS identifier.
    pub protocol: Protocol,
    /// Authentication declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Auth plugin identity as a scalar column (`auth_plugin_ref`), so the
    /// plugin-in-use check never depends on JSON scanning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_plugin_ref: Option<String>,
    /// Parsed UUID of the auth plugin reference when it is UUID-backed
    /// (`auth_plugin_uuid`); `None` for a named plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_plugin_uuid: Option<Uuid>,
    /// Header manipulation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Plugin pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Creation instant, stamped by the store.
    pub created_at: Timestamp,
}
// @cpt-end:cpt-cf-oagw-dod-upstream-model:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-route-model:p1:inst-full
/// A route: a match rule that binds a request path to an upstream.
///
/// `id`, `tenant_id` and `upstream_id` are immutable; `enabled`, `priority` and
/// the match itself are replaced wholesale by `cpt-cf-oagw-flow-route-replace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Immutable identity.
    pub id: Uuid,
    /// Owning tenant; immutable and the key of every store lookup.
    pub tenant_id: Uuid,
    /// Upstream this route binds to; immutable.
    pub upstream_id: Uuid,
    /// Whether the route matches traffic; defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match rule, exactly one of `http` and `grpc` (`match` in the schema).
    #[serde(rename = "match")]
    pub matches: MatchRule,
    /// Match kind derived from [`Self::matches`].
    pub match_type: MatchType,
    /// Match precedence; higher wins. Defaults to `0`.
    #[serde(default)]
    pub priority: i32,
    /// Free-form labels (`^[a-z0-9_-]+$`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Plugin pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS declaration (recorded deviation: routes carry their own `cors`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation instant, stamped by the store.
    pub created_at: Timestamp,
}
// @cpt-end:cpt-cf-oagw-dod-route-model:p1:inst-full

/// Route match rule, exactly one of the two members (`route.match`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchRule {
    /// HTTP match; exclusive with [`Self::grpc`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match; exclusive with [`Self::http`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchRule {
    /// The kind of match this rule declares.
    #[must_use]
    pub fn match_type(&self) -> Option<MatchType> {
        if self.http.is_some() {
            Some(MatchType::Http)
        } else if self.grpc.is_some() {
            Some(MatchType::Grpc)
        } else {
            None
        }
    }
}

/// HTTP match (`match.http`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods the route matches; at least one.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path prefix; at least one character.
    #[serde(default)]
    pub path: String,
    /// Query parameters matched on; defaults to empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// What happens to the unmatched suffix; defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: SuffixMode,
}

/// gRPC match (`match.grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    #[serde(default)]
    pub service: String,
    /// Method name.
    #[serde(default)]
    pub method: String,
}

/// `true`, the schema default of `enabled`.
#[must_use]
const fn default_true() -> bool {
    true
}

/// A plugin definition.
///
/// The DESIGN class and the ADR 0002 definition examples fix the field set
/// (`cpt-cf-oagw-dod-plugin-model`). `id`, `tenant_id`, `last_used_at` and
/// `gc_eligible_at` are server-managed, the last two left unset in this
/// deployment because usage tracking and the GC job are out of scope. The whole
/// definition is immutable after creation: no replace operation exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// Immutable identity: the anonymous GTS identifier
    /// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`, generated on create.
    pub id: String,
    /// Owning tenant, from the security context; immutable.
    pub tenant_id: Uuid,
    /// Plugin type in `auth|guard|transform`; immutable and never changed.
    pub plugin_type: crate::domain::plugin::PluginType,
    /// Tenant-unique name (`UNIQUE (tenant_id, name)`).
    pub name: String,
    /// Free-form description of what the plugin does.
    #[serde(default)]
    pub description: String,
    /// Declared configuration contract, stored verbatim as a JSON object.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config_schema: serde_json::Value,
    /// Phases the plugin declares, in `on_request|on_response|on_error`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<crate::domain::plugin::Phase>,
    /// Stored Starlark source, verbatim and without any syntax check.
    #[serde(default)]
    pub source_code: String,
    /// Last-use instant; unset, because no usage tracking exists in this
    /// deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<Timestamp>,
    /// Garbage-collection eligibility instant; unset, because no GC job exists
    /// in this deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<Timestamp>,
}

impl Plugin {
    /// The UUID instance part of the definition's GTS identifier.
    #[must_use]
    pub fn uuid(&self) -> Option<Uuid> {
        crate::domain::plugin::definition_uuid(&self.id)
    }

    /// The base identifier of the definition's type.
    #[must_use]
    pub const fn base_identifier(&self) -> &'static str {
        self.plugin_type.base_identifier()
    }
}

impl toolkit::DomainModel for Upstream {}

impl toolkit::DomainModel for Route {}

impl toolkit::DomainModel for Plugin {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trips_through_rfc3339() {
        let stamp = Timestamp::from_nanos(1_700_000_000_123_000_000);
        let rendered = stamp.to_rfc3339();
        assert_eq!(rendered, "2023-11-14T22:13:20.123Z");
        let parsed = Timestamp::parse_rfc3339(&rendered).expect("parses its own rendering");
        assert_eq!(parsed, stamp);
    }

    #[test]
    fn timestamp_renders_the_epoch_and_leap_days() {
        assert_eq!(Timestamp::from_nanos(0).to_rfc3339(), "1970-01-01T00:00:00.000Z");
        // 2024-02-29 exists: the civil calendar used here knows leap days.
        let leap = Timestamp::parse_rfc3339("2024-02-29T00:00:00.000Z").expect("leap day");
        assert_eq!(leap.to_rfc3339(), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn timestamp_rejects_malformed_values() {
        assert!(Timestamp::parse_rfc3339("not-a-date").is_none());
        assert!(Timestamp::parse_rfc3339("2024-13-01T00:00:00.000Z").is_none());
        assert!(Timestamp::parse_rfc3339("2024-01-01 00:00:00.000Z").is_none());
    }

    #[test]
    fn timestamp_orders_by_instant() {
        assert!(Timestamp::from_nanos(1) < Timestamp::from_nanos(2));
        assert!(Timestamp::now().as_nanos() > 0);
    }

    #[test]
    fn schemes_report_their_schema_names_and_default_ports() {
        assert_eq!(Scheme::Http.as_str(), "http");
        assert_eq!(Scheme::Http.default_port(), 80);
        assert_eq!(Scheme::Https.default_port(), 443);
        assert_eq!(Scheme::parse("wss"), Some(Scheme::Wss));
        assert_eq!(Scheme::parse("httpx"), None);
    }

    #[test]
    fn protocols_round_trip_through_their_gts_identifiers() {
        assert_eq!(Protocol::parse(PROTOCOL_HTTP), Some(Protocol::Http));
        assert_eq!(Protocol::Http.as_str(), PROTOCOL_HTTP);
        assert_eq!(Protocol::Grpc.to_string(), PROTOCOL_GRPC);
        assert_eq!(Protocol::parse("gts.cf.core.oagw.protocol.v1~bogus.v1"), None);
    }

    #[test]
    fn rate_limit_resolves_the_burst_default_to_the_sustained_rate() {
        let declared = RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 10,
                window: RateWindow::Second,
            },
            burst: Some(BurstRate { capacity: Some(20) }),
            scope: RateScope::Route,
            strategy: RateStrategy::Reject,
            cost: 1,
        };
        assert_eq!(declared.effective_capacity(), 20);
        let defaulted = RateLimitConfig {
            burst: Some(BurstRate { capacity: None }),
            ..declared
        };
        assert_eq!(defaulted.effective_capacity(), 10);
        let absent = RateLimitConfig { burst: None, ..declared };
        assert_eq!(absent.effective_capacity(), 10);
    }

    #[test]
    fn endpoint_pool_keeps_only_the_protocol_and_port_peers() {
        let server = ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: Scheme::Https,
                    host: "a.internal".to_string(),
                    port: 443,
                },
                Endpoint {
                    scheme: Scheme::Https,
                    host: "b.internal".to_string(),
                    port: 443,
                },
                Endpoint {
                    scheme: Scheme::Grpc,
                    host: "c.internal".to_string(),
                    port: 443,
                },
                Endpoint {
                    scheme: Scheme::Https,
                    host: "d.internal".to_string(),
                    port: 8443,
                },
            ],
        };
        let pool = server.pool();
        assert_eq!(pool.len(), 2);
        assert_eq!(pool[0].host, "a.internal");
        assert_eq!(pool[1].host, "b.internal");
    }

    #[test]
    fn empty_server_pool_is_empty() {
        assert!(ServerConfig { endpoints: Vec::new() }.pool().is_empty());
    }

    #[test]
    fn match_rule_derives_its_kind() {
        let http = MatchRule {
            http: Some(HttpMatch {
                methods: vec!["GET".to_string()],
                path: "/v1".to_string(),
                query_allowlist: Vec::new(),
                path_suffix_mode: SuffixMode::Append,
            }),
            grpc: None,
        };
        assert_eq!(http.match_type(), Some(MatchType::Http));
        let neither = MatchRule { http: None, grpc: None };
        assert_eq!(neither.match_type(), None);
    }

    #[test]
    fn enums_render_their_schema_names() {
        assert_eq!(Sharing::Inherit.as_str(), "inherit");
        assert_eq!(RateWindow::Hour.as_str(), "hour");
        assert_eq!(RateWindow::Hour.seconds(), 3_600);
        assert_eq!(RateAlgorithm::TokenBucket.to_string(), "token_bucket");
        assert_eq!(RateScope::User.to_string(), "user");
        assert_eq!(RateStrategy::Degrade.to_string(), "degrade");
        assert_eq!(HeaderPassthrough::Allowlist.to_string(), "allowlist");
        assert_eq!(SuffixMode::Disabled.to_string(), "disabled");
        assert_eq!(MatchType::Grpc.to_string(), "grpc");
        assert_eq!(CorsMethod::Options.as_str(), "OPTIONS");
        assert_eq!(HttpMethod::Patch.to_string(), "PATCH");
    }

    #[test]
    fn sharing_and_headers_parse_their_schema_names() {
        assert_eq!(Sharing::parse("enforce"), Some(Sharing::Enforce));
        assert_eq!(Sharing::parse("platform"), None);
        assert_eq!(HeaderPassthrough::parse("allowlist"), Some(HeaderPassthrough::Allowlist));
        assert_eq!(HeaderPassthrough::parse("everything"), None);
        assert_eq!(RateWindow::parse("day"), Some(RateWindow::Day));
        assert_eq!(RateWindow::parse("week"), None);
        assert_eq!(RateAlgorithm::parse("sliding_window"), Some(RateAlgorithm::SlidingWindow));
        assert_eq!(RateScope::parse("tenant"), Some(RateScope::Tenant));
        assert_eq!(RateStrategy::parse("reject"), Some(RateStrategy::Reject));
        assert_eq!(RateStrategy::parse("throttle"), None);
        assert_eq!(SuffixMode::parse("append"), Some(SuffixMode::Append));
    }

    #[test]
    fn timestamp_serializes_as_an_rfc3339_string() {
        let stamp = Timestamp::from_nanos(1_700_000_000_000_000_000);
        let encoded = serde_json::to_value(stamp).expect("serializes");
        assert_eq!(encoded.as_str(), Some("2023-11-14T22:13:20.000Z"));
        let decoded: Timestamp = serde_json::from_value(encoded).expect("deserializes");
        assert_eq!(decoded, stamp);
    }

    #[test]
    fn upstream_serializes_with_the_schema_member_names() {
        let upstream = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            enabled: true,
            alias: "payments".to_string(),
            tags: vec!["core".to_string()],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "payments.internal".to_string(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::from_nanos(1_700_000_000_000_000_000),
        };
        let encoded = serde_json::to_value(&upstream).expect("serializes");
        assert_eq!(encoded["alias"], "payments");
        assert_eq!(encoded["protocol"], PROTOCOL_HTTP);
        assert_eq!(encoded["server"]["endpoints"][0]["scheme"], "https");
        assert_eq!(encoded["server"]["endpoints"][0]["port"], 443);
        assert_eq!(encoded["created_at"], "2023-11-14T22:13:20.000Z");
        // Absent optional members are omitted rather than serialized as null.
        assert!(encoded.get("auth").is_none());
        assert!(encoded.get("rate_limit").is_none());
    }

    #[test]
    fn route_serializes_its_match_and_defaults() {
        let route = Route {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            enabled: false,
            matches: MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["POST".to_string(), "GET".to_string()],
                    path: "/v1/pay".to_string(),
                    query_allowlist: vec!["acct".to_string()],
                    path_suffix_mode: SuffixMode::Disabled,
                }),
                grpc: None,
            },
            match_type: MatchType::Http,
            priority: 7,
            tags: vec!["payments".to_string()],
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: Timestamp::from_nanos(0),
        };
        let encoded = serde_json::to_value(&route).expect("serializes");
        assert_eq!(encoded["match"]["http"]["path"], "/v1/pay");
        assert_eq!(encoded["match_type"], "http");
        assert_eq!(encoded["priority"], 7);
        assert_eq!(encoded["match"]["http"]["path_suffix_mode"], "disabled");
        assert!(encoded.get("grpc").is_none());
    }

    #[test]
    fn an_unknown_member_is_rejected_when_deserializing_a_stored_record() {
        let raw = serde_json::json!({
            "id": Uuid::nil(),
            "tenant_id": Uuid::nil(),
            "alias": "payments",
            "server": { "endpoints": [] },
            "protocol": PROTOCOL_HTTP,
            "created_at": "1970-01-01T00:00:00.000Z",
            "surprise": true
        });
        let decoded: Result<Upstream, _> = serde_json::from_value(raw);
        assert!(decoded.is_err(), "deny_unknown_fields guards the record");
    }
}
