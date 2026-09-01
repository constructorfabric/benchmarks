//! Deterministic control-plane validation and alias derivation
//! (DESIGN section 3.1 alias rules, section 3.3 CRUD semantics).
//!
//! The [`Validator`] is pure: it takes the operator configuration it needs
//! ([`OagwConfig::allow_http_upstream`]) plus one resource draft and either
//! returns the populated domain model or an [`OagwError`] carrying the GTS
//! problem type, HTTP status and extension fields DESIGN section 3.3 assigns.
//! Nothing here touches the store, so every rule is unit-testable in
//! isolation and the control-plane service composes them.
//!
//! ## Alias derivation
//!
//! The alias is a routing key, not a label, so it is *derived* from the
//! endpoint pool whenever the pool makes that possible:
//!
//! | Endpoint pool | Derived alias |
//! |---|---|
//! | single hostname, standard port | hostname |
//! | single hostname, non-standard port | `hostname:port` |
//! | several hostnames, registrable common suffix (>= 2 labels) | common suffix |
//! | several hostnames, registrable common suffix, non-standard port | `suffix:port` |
//! | mixed / IPv4 / IPv6 endpoints | not derivable -> explicit alias required |
//!
//! A hostname-based pool always derives: a user-supplied alias that differs
//! from the derived value is rejected (400) while the exact derived value is
//! accepted silently (idempotent). A pool whose only common suffix is a bare
//! public suffix (`co.uk`) is *not* derivable — the public-suffix list decides
//! (the `psl` crate). Non-derivable pools require an explicit alias.
//!
//! ## Immutability
//!
//! `id` and `tenant_id` are server-generated, and the alias is immutable once
//! set (it is the routing key in `/oagw/v1/proxy/{alias}/...`): a replace whose
//! recomputed alias differs from the stored one is rejected, and a pool that
//! becomes non-derivable is rejected outright.
//!
//! ## Plugin bindings
//!
//! A draft that binds a plugin (`auth.type`, `plugins.items[].plugin_ref`) is
//! resolved at bind time against the [`PluginCatalog`] the caller passes in:
//! a builtin id, or a custom plugin of the calling tenant in either wire
//! spelling (bare UUID or `gts.cf.core.oagw.plugin.v1~{uuid}`). An
//! unresolvable reference is a `400` here rather than a `503`
//! `plugin.not_found` per request on the data plane. The catalog covers the
//! calling tenant's own plugins: ancestor resolution is asynchronous, so a
//! descendant binds only the builtins and its own custom plugins (see
//! `ControlPlaneService::plugin_catalog`).

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::Arc;

use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, HttpMethod, Plugin, PluginConfig, Protocol,
    RateLimitConfig, Route, RouteMatch, Scheme, ServerConfig, Upstream, parse_plugin_id,
};

/// Longest RFC 1123 hostname (excluding a trailing dot).
pub const MAX_HOSTNAME_LEN: usize = 253;

/// Longest single DNS label.
pub const MAX_LABEL_LEN: usize = 63;

/// Most tags a resource may carry.
///
/// The JSON schemas bound only the tag *spelling*; this cap keeps a discovery
/// index from growing without bound. Tags are stored verbatim, not deduplicated.
pub const MAX_TAG_COUNT: usize = 32;

/// Longest single tag.
pub const MAX_TAG_LEN: usize = 64;

/// Default port for the TLS-based schemes (`https`, `wss`, `wt`, `grpc`).
pub const STANDARD_TLS_PORT: u16 = 443;

/// Default port for the plaintext schemes (`http`, `ws`).
pub const STANDARD_PLAINTEXT_PORT: u16 = 80;

/// Minimum port value the endpoint schema accepts.
pub const MIN_PORT: u16 = 1;

/// Minimum number of labels a common suffix needs to be a routing alias.
pub const MIN_SUFFIX_LABELS: usize = 2;

/// Characters forbidden in an endpoint host (userinfo, path, query, fragment
/// and percent-encoding introducers).
const FORBIDDEN_HOST_CHARS: &[char] = &['/', '\\', '?', '#', '@', '%'];

/// Upstream creation/replace draft: everything the caller supplies, with the
/// server-generated fields absent.
#[derive(Debug, Clone)]
pub struct UpstreamInput {
    /// Alias as supplied on the wire; `None` when omitted.
    pub alias: Option<String>,
    /// `enabled` flag; `None` falls back to the schema default (`true`).
    pub enabled: Option<bool>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Auth plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: HeadersConfig,
    /// Plugin chain.
    pub plugins: PluginConfig,
    /// Rate-limit budget.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
}

/// Route creation/replace draft. `upstream_id` is deliberately absent: it is
/// immutable and the control-plane service injects it (from the request body on
/// create, from the stored route on replace).
#[derive(Debug, Clone)]
pub struct RouteInput {
    /// Match rules; exactly one of `http` / `grpc`.
    pub r#match: RouteMatch,
    /// Header transformation overrides.
    pub headers: HeadersConfig,
    /// Plugin chain.
    pub plugins: PluginConfig,
    /// Route-level rate-limit override.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    pub cors: Option<CorsConfig>,
    /// `enabled` flag; `None` falls back to the schema default (`true`).
    pub enabled: Option<bool>,
    /// Match priority; `None` falls back to `0`.
    pub priority: Option<i32>,
    /// Discovery tags.
    pub tags: Vec<String>,
}

/// Plugin creation draft. Plugins are immutable, so there is no replace draft.
#[derive(Debug, Clone)]
pub struct PluginInput {
    /// Plugin base type GTS id (e.g. `gts.cf.core.oagw.guard_plugin.v1`).
    pub plugin_type: String,
    /// Plugin configuration payload (must be a JSON object).
    pub config: serde_json::Value,
    /// `enabled` flag; `None` falls back to `true`.
    pub enabled: Option<bool>,
    /// Discovery tags.
    pub tags: Vec<String>,
}

/// A plugin the calling tenant may bind: its instance id and the base type it
/// was registered as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogPlugin {
    id: Uuid,
    /// GTS base type of the plugin, absent when the caller only had ids to
    /// hand over (see [`PluginCatalog::of`]).
    plugin_type: Option<String>,
}

/// The plugins a caller may bind, as seen by [`Validator::validate_bindings`].
///
/// The [`Validator`] stays pure, so the caller (the control-plane service, the
/// only place that knows the tenant) hands in the catalogue instead of the
/// validator reaching into the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginCatalog {
    /// Custom plugins the calling tenant owns.
    plugins: Vec<CatalogPlugin>,
}

impl PluginCatalog {
    /// Builds a catalogue from the custom plugin ids of the calling tenant.
    ///
    /// Only identities are recorded, so the *slot* of a binding is not checked
    /// against the plugin's kind (see [`PluginSlot`]). Production callers build
    /// the catalogue with [`PluginCatalog::of_plugins`], which carries the base
    /// type and enables that check.
    #[must_use]
    pub fn of<I: IntoIterator<Item = Uuid>>(plugin_ids: I) -> Self {
        Self {
            plugins: plugin_ids
                .into_iter()
                .map(|id| CatalogPlugin {
                    id,
                    plugin_type: None,
                })
                .collect(),
        }
    }

    /// Builds a catalogue from the plugins the calling tenant owns, recording
    /// the base type each one was created as.
    #[must_use]
    pub fn of_plugins(plugins: &[Arc<Plugin>]) -> Self {
        Self {
            plugins: plugins
                .iter()
                .map(|plugin| CatalogPlugin {
                    id: plugin.id,
                    plugin_type: Some(plugin.plugin_type.clone()),
                })
                .collect(),
        }
    }

    /// `true` when `reference` resolves to a bindable plugin: a builtin the
    /// registry implements, or a custom plugin of the calling tenant in either
    /// wire spelling (bare UUID or GTS-form id).
    #[must_use]
    pub fn resolves(&self, reference: &str) -> bool {
        if is_builtin_plugin_ref(reference) {
            return true;
        }
        self.custom_instance(reference).is_some()
    }

    /// The GTS base type `reference` was registered as, when the catalogue can
    /// tell.
    ///
    /// A built-in carries its base type inside its own id (the bare
    /// instance-fragment spelling resolves through [`BUILTIN_PLUGIN_REFS`]); a
    /// custom plugin carries the base type it was created with. `None` means
    /// the reference does not resolve to a known plugin, or its kind is not
    /// recorded, in which case the slot check cannot run.
    #[must_use]
    fn kind_of(&self, reference: &str) -> Option<String> {
        if let Some(base) = builtin_base_type(reference) {
            return Some(base.to_owned());
        }
        self.custom_instance(reference)?.1
    }

    /// The `(id, recorded base type)` of the custom plugin `reference` names, in
    /// either wire spelling.
    fn custom_instance(&self, reference: &str) -> Option<(Uuid, Option<String>)> {
        let instance = match Uuid::parse_str(reference) {
            Ok(id) => id,
            Err(_) => parse_plugin_id(reference)?,
        };
        self.plugins
            .iter()
            .find(|plugin| plugin.id == instance)
            .map(|plugin| (plugin.id, plugin.plugin_type.clone()))
    }
}

/// The slot a plugin binding sits in, and the plugin kinds that fit it.
///
/// DESIGN §3.4 models the association as `Upstream "1" --> "0..1" Plugin` for
/// the *auth* binding and `Upstream/Route "1" --> "*" Plugin` for the chains,
/// and ADR-0002 types the plugins accordingly: an auth slot takes an
/// `auth_plugin`, a chain slot takes a `guard_plugin` or a `transform_plugin`.
/// The engine resolves the three kinds through separate registries, so a
/// reference bound in the wrong slot would only ever surface as a `503
/// plugin.not_found` on the data plane; validating the kind at bind time turns
/// that into a `400` that names the slot instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSlot {
    /// The `auth` binding of an upstream.
    Auth,
    /// A binding of an upstream or route plugin chain.
    Chain,
}

impl PluginSlot {
    /// Human-readable name of the slot, for the problem detail.
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Chain => "plugins",
        }
    }

    /// The base types the slot accepts.
    #[must_use]
    fn accepts(self) -> &'static [&'static str] {
        match self {
            Self::Auth => &[crate::domain::plugin::AUTH_PLUGIN_TYPE_ID],
            Self::Chain => &[
                crate::domain::plugin::GUARD_PLUGIN_TYPE_ID,
                crate::domain::plugin::TRANSFORM_PLUGIN_TYPE_ID,
            ],
        }
    }
}

/// The GTS base type of a built-in plugin reference, in either spelling.
#[must_use]
fn builtin_base_type(reference: &str) -> Option<&'static str> {
    BUILTIN_PLUGIN_REFS.iter().find_map(|gts_id| {
        let (base, instance) = gts_id.split_once('~')?;
        (*gts_id == reference || instance == reference).then_some(base)
    })
}

/// GTS instance fragments of the built-in plugins that actually exist
/// ([`PluginRegistry::with_builtins`](crate::infra::plugin::PluginRegistry::with_builtins),
/// ADR-0002).
///
/// The full GTS ids live in [`crate::domain::plugin::builtin`]; the fragment is
/// what an operator writes in a binding. The catalog-only identifiers (`basic`,
/// `bearer`, `timeout`, `cors`, `logging`, `metrics`) are deliberately absent:
/// they have no backing implementation, so binding one is a `400` here rather
/// than a `503` at proxy time.
pub const BUILTIN_PLUGIN_REFS: &[&str] = &[
    crate::domain::plugin::builtin::NOOP_AUTH,
    crate::domain::plugin::builtin::APIKEY_AUTH,
    crate::domain::plugin::builtin::OAUTH2_CLIENT_CRED,
    crate::domain::plugin::builtin::OAUTH2_CLIENT_CRED_BASIC,
    crate::domain::plugin::builtin::REQUIRED_HEADERS_GUARD,
    crate::domain::plugin::builtin::REQUEST_ID_TRANSFORM,
];

/// `true` when `reference` names a built-in plugin, in the full GTS spelling or
/// in the bare instance-fragment spelling (`cf.core.oagw.apikey.v1`).
#[must_use]
fn is_builtin_plugin_ref(reference: &str) -> bool {
    builtin_base_type(reference).is_some()
}

/// `true` when `tag` matches the schema pattern `^[a-z0-9_-]+$`.
#[must_use]
fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

/// How a host was classified, used to decide whether an alias is derivable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKind {
    /// RFC 1123 hostname; the payload is the lowercased, trailing-dot-free
    /// spelling used for alias derivation.
    Hostname(String),
    /// Dotted-quad IPv4 address.
    Ipv4,
    /// IPv6 address (bracketed or inline).
    Ipv6,
}

/// Identity of a route's match rule for uniqueness checks (DESIGN section 3.3:
/// "same path + priority + method -> 409").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchKey {
    /// HTTP match: path pattern plus the set of allowed methods.
    Http {
        /// Path pattern.
        path: String,
        /// Allowed methods, order-insensitive.
        methods: BTreeSet<HttpMethod>,
    },
    /// gRPC match: service plus method.
    Grpc {
        /// Fully qualified service name.
        service: String,
        /// RPC method name.
        method: String,
    },
}

/// Match-rule key of a route, used by both the service and the registry.
#[must_use]
pub fn route_match_key(route: &Route) -> MatchKey {
    match (&route.r#match.http, &route.r#match.grpc) {
        (Some(http), _) => MatchKey::Http {
            path: http.path.clone(),
            methods: http.methods.iter().copied().collect(),
        },
        (None, Some(grpc)) => MatchKey::Grpc {
            service: grpc.service.clone(),
            method: grpc.method.clone(),
        },
        // An unvalidated route cannot reach the store; a stable key keeps the
        // registry total without inventing a third variant.
        (None, None) => MatchKey::Grpc {
            service: String::new(),
            method: String::new(),
        },
    }
}

/// `true` when `scheme`/`port` is the scheme's standard port, which the derived
/// alias omits.
#[must_use]
pub const fn is_standard_port(scheme: Scheme, port: u16) -> bool {
    match scheme {
        Scheme::Http | Scheme::Ws => port == STANDARD_PLAINTEXT_PORT,
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => port == STANDARD_TLS_PORT,
    }
}

/// `true` when the scheme is accepted under the given configuration.
///
/// `https`/`wss`/`wt`/`grpc` are always legal; `http`/`ws` are only legal when
/// the operator allows plaintext upstreams.
#[must_use]
pub const fn is_scheme_allowed(scheme: Scheme, allow_http_upstream: bool) -> bool {
    match scheme {
        Scheme::Http | Scheme::Ws => allow_http_upstream,
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => true,
    }
}

/// `true` when `alias` matches the upstream schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` and is not reserved.
///
/// One alias is reserved: the `unmatched` literal the metrics fold every
/// unresolved request onto ([`crate::domain::metrics::UNMATCHED_HOST`]). An
/// upstream that claimed it would share a metric series with requests that
/// never resolved anywhere, so the label it reports could no longer be trusted.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    if alias == crate::domain::metrics::UNMATCHED_HOST {
        return false;
    }
    let bytes = alias.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    let Some((&last, middle)) = rest.split_last() else {
        return first.is_ascii_lowercase() || first.is_ascii_digit();
    };
    let is_edge = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let is_inner = |byte: &u8| is_edge(byte) || matches!(byte, b'.' | b':' | b'-');
    is_edge(&first) && is_edge(&last) && middle.iter().all(is_inner)
}

/// Normalises an alias: ASCII lowercase, surrounding whitespace removed,
/// trailing dots stripped (DESIGN section 3.1 alias normalisation).
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// `true` when `candidate` looks like a GTS *type* identifier
/// (`gts.<domain…>.<type>.v1`, lower-case, no instance part).
#[must_use]
pub fn is_gts_type_id(candidate: &str) -> bool {
    let Some(rest) = candidate.strip_prefix("gts.") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let shape_ok = candidate
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    shape_ok && rest.split('.').count() >= 3
}

/// Classifies and validates an endpoint host.
///
/// # Errors
///
/// Returns a human-readable reason when the host is not an RFC 1123 hostname,
/// an IPv4 address or an IPv6 literal — i.e. when it carries a port, path,
/// userinfo, whitespace, an empty label or a malformed IP literal.
pub fn classify_host(raw: &str) -> Result<HostKind, String> {
    if raw.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    if raw.chars().any(char::is_whitespace) {
        return Err("host must not contain whitespace".to_owned());
    }
    if let Some(bad) = raw.chars().find(|c| FORBIDDEN_HOST_CHARS.contains(c)) {
        return Err(format!("host must not contain {bad:?}"));
    }

    // A single trailing dot is FQDN notation and tolerated (DESIGN section 3.1).
    let host = raw.strip_suffix('.').unwrap_or(raw);
    if host.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(format!("host exceeds {MAX_HOSTNAME_LEN} characters"));
    }

    if host.contains(':') {
        // Bracketed or inline IPv6 only: a hostname may never carry a port.
        let inner = host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(host);
        return Ipv6Addr::from_str(inner)
            .map(|_parsed| HostKind::Ipv6)
            .map_err(|_| "host must be an IPv6 literal without a port".to_owned());
    }

    let labels: Vec<&str> = host.split('.').collect();
    if labels
        .iter()
        .all(|label| !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_digit()))
    {
        // All-numeric dotted form: only a valid IPv4 address is acceptable, so
        // `256.1.1.1` is rejected instead of being read as a hostname.
        if labels.len() != 4 {
            return Err("all-numeric host must be a dotted-quad IPv4 address".to_owned());
        }
        return Ipv4Addr::from_str(host)
            .map(|_parsed| HostKind::Ipv4)
            .map_err(|_| "host is not a valid IPv4 address".to_owned());
    }

    for label in &labels {
        if label.is_empty() {
            return Err("host must not contain empty labels".to_owned());
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(format!("host label exceeds {MAX_LABEL_LEN} characters"));
        }
        let invalid = label
            .bytes()
            .find(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'-'));
        if let Some(byte) = invalid {
            return Err(format!(
                "host label contains the unsupported character {:?}",
                byte as char
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("host label must not start or end with a hyphen".to_owned());
        }
    }

    Ok(HostKind::Hostname(host.to_ascii_lowercase()))
}

/// Common suffix of several label lists, longest first, `None` when the hosts
/// share nothing with at least [`MIN_SUFFIX_LABELS`] labels or when the only
/// shared suffix is a bare public suffix.
fn common_suffix(hosts: &[Vec<String>]) -> Option<String> {
    let shortest = hosts.iter().map(Vec::len).min()?;
    let mut best: Option<usize> = None;
    for count in (MIN_SUFFIX_LABELS..=shortest).rev() {
        let candidate: &[String] = &hosts[0][hosts[0].len() - count..];
        let shared = hosts
            .iter()
            .all(|labels| labels[labels.len() - count..] == *candidate);
        if shared {
            best = Some(count);
            break;
        }
    }
    let count = best?;
    let suffix = hosts[0][hosts[0].len() - count..].join(".");
    // A bare public suffix (`co.uk`) is not a registrable domain, so it must
    // not become a routing alias (DESIGN section 3.1).
    if is_bare_public_suffix(&suffix) {
        return None;
    }
    Some(suffix)
}

/// `true` when the public-suffix list knows `candidate` as a public suffix and
/// it is not itself a registrable domain.
fn is_bare_public_suffix(candidate: &str) -> bool {
    psl::suffix_str(candidate).is_some() && psl::domain_str(candidate).is_none()
}

/// Deterministic textual rendering of the plugin definition returned by
/// `GET /oagw/v1/plugins/{id}/source`.
///
/// Starlark-flavoured assignment lines plus a pretty-printed configuration
/// object, sorted by key so the rendering is byte-stable for byte-equal
/// definitions.
#[must_use]
pub fn render_plugin_source(
    plugin_id: &str,
    plugin_type: &str,
    tenant_id: &Uuid,
    enabled: bool,
    tags: &[String],
    config: &serde_json::Value,
) -> String {
    let tags = tags
        .iter()
        .map(|tag| format!("{tag:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut lines = vec![
        format!("# plugin_id: {plugin_id}"),
        format!("# tenant_id: {tenant_id}"),
        format!("PLUGIN_TYPE = {plugin_type:?}"),
        format!("ENABLED = {}", if enabled { "True" } else { "False" }),
        format!("TAGS = [{tags}]"),
        "CONFIG = ".to_owned(),
    ];
    if let Ok(pretty) = serde_json::to_string_pretty(config) {
        lines.push(pretty);
    } else {
        lines.push("{}".to_owned());
    }
    let mut rendered = lines.join("\n");
    rendered.push('\n');
    rendered
}

/// Applies the DESIGN section 3.1 / 3.3 validation rules.
#[derive(Debug, Clone)]
pub struct Validator {
    config: OagwConfig,
}

impl Validator {
    /// Builds a validator bound to the effective gear configuration.
    #[must_use]
    pub const fn new(config: OagwConfig) -> Self {
        Self { config }
    }

    /// The configuration the validator enforces.
    #[must_use]
    pub const fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// Validates the endpoint pool.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the pool is empty, when any
    /// endpoint host/scheme/port is invalid, or when the pool mixes schemes or
    /// ports.
    pub fn validate_pool(&self, endpoints: &[Endpoint]) -> Result<(), OagwError> {
        if endpoints.is_empty() {
            return Err(OagwError::validation(
                "field `server.endpoints`: at least one endpoint is required",
            ));
        }
        let first = &endpoints[0];
        for endpoint in endpoints {
            self.validate_endpoint(endpoint)?;
            if endpoint.scheme != first.scheme {
                return Err(OagwError::validation(format!(
                    "field `server.endpoints`: every endpoint must use the same scheme; `{}` uses `{:?}` while the first uses `{:?}`",
                    endpoint.host, endpoint.scheme, first.scheme
                ))
                .with_host(endpoint.host.as_str()));
            }
            if endpoint.port != first.port {
                return Err(OagwError::validation(format!(
                    "field `server.endpoints`: every endpoint must use the same port; `{}` uses {} while the first uses {}",
                    endpoint.host, endpoint.port, first.port
                ))
                .with_host(endpoint.host.as_str()));
            }
        }
        Ok(())
    }

    /// Validates one endpoint (scheme gate, host shape, port range).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] describing the offending field.
    pub fn validate_endpoint(&self, endpoint: &Endpoint) -> Result<(), OagwError> {
        if !is_scheme_allowed(endpoint.scheme, self.config.allow_http_upstream) {
            return Err(OagwError::validation(format!(
                "field `server.endpoints[].scheme`: scheme `{:?}` requires `allow_http_upstream: true`",
                endpoint.scheme
            ))
            .with_host(endpoint.host.as_str())
            .with_invalid_value(format!("{:?}", endpoint.scheme).to_ascii_lowercase()));
        }
        if endpoint.port < MIN_PORT {
            return Err(OagwError::validation(format!(
                "field `server.endpoints[].port`: port must be between {MIN_PORT} and 65535"
            ))
            .with_host(endpoint.host.as_str())
            .with_invalid_value(endpoint.port.to_string()));
        }
        match classify_host(&endpoint.host) {
            Ok(_) => Ok(()),
            Err(reason) => Err(OagwError::validation(format!(
                "field `server.endpoints[].host`: {reason}"
            ))
            .with_host(endpoint.host.as_str())
            .with_invalid_value(endpoint.host.as_str())),
        }
    }

    /// Derives the alias of an endpoint pool, `None` when the pool is not
    /// derivable (IP endpoints, mixed hosts, bare-public-suffix pools).
    ///
    /// The caller must have validated the pool first
    /// ([`Validator::validate_pool`]): the first endpoint supplies the scheme
    /// and port the derivation consults.
    #[must_use]
    pub fn derive_alias(&self, endpoints: &[Endpoint]) -> Option<String> {
        let first = endpoints.first()?;
        let standard = is_standard_port(first.scheme, first.port);
        let mut hosts: Vec<Vec<String>> = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            match classify_host(&endpoint.host) {
                Ok(HostKind::Hostname(host)) => {
                    hosts.push(host.split('.').map(ToOwned::to_owned).collect());
                }
                _ => return None,
            }
        }
        let core = if hosts.len() == 1 {
            hosts[0].join(".")
        } else {
            common_suffix(&hosts)?
        };
        Some(if standard {
            core
        } else {
            format!("{core}:{}", first.port)
        })
    }

    /// Resolves the alias of a *new* upstream.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the pool is derivable and the
    /// supplied alias differs from the derived value, when the pool is not
    /// derivable and no (or an invalid) alias was supplied, or when the resolved
    /// alias is not a valid one — a *derived* alias is checked too, because the
    /// hostname the pool is derived from decides it and could otherwise name a
    /// reserved word.
    pub fn resolve_create_alias(
        &self,
        endpoints: &[Endpoint],
        supplied: Option<&str>,
    ) -> Result<String, OagwError> {
        let derived = self.derive_alias(endpoints);
        match derived {
            Some(derived) => {
                if let Some(supplied) = supplied.map(normalize_alias)
                    && supplied != derived
                {
                    return Err(OagwError::validation(format!(
                        "field `alias`: hostname-based endpoints always derive the alias; expected `{derived}`"
                    ))
                    .with_alias(derived.as_str())
                    .with_invalid_value(supplied));
                }
                self.check_alias_shape(&derived)?;
                Ok(derived)
            }
            None => {
                let supplied = supplied.map(normalize_alias).ok_or_else(|| {
                    OagwError::validation(
                        "field `alias`: an explicit alias is required for IP-based or non-derivable endpoint pools",
                    )
                })?;
                self.check_alias_shape(&supplied)?;
                Ok(supplied)
            }
        }
    }

    /// Resolves the alias of a *replaced* upstream, enforcing alias
    /// immutability (DESIGN section 3.1 alias update behaviour).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the recomputed alias would
    /// change, when the pool turns non-derivable, or when the supplied alias
    /// differs from the derived/stored value.
    pub fn resolve_replace_alias(
        &self,
        stored_alias: &str,
        stored_endpoints: &[Endpoint],
        endpoints: &[Endpoint],
        supplied: Option<&str>,
    ) -> Result<String, OagwError> {
        let was_derivable = self.derive_alias(stored_endpoints).is_some();
        match self.derive_alias(endpoints) {
            Some(derived) => {
                if let Some(supplied) = supplied.map(normalize_alias)
                    && supplied != derived
                {
                    return Err(OagwError::validation(format!(
                        "field `alias`: hostname-based endpoints always derive the alias; expected `{derived}`"
                    ))
                    .with_alias(derived.as_str())
                    .with_invalid_value(supplied));
                }
                if derived != stored_alias {
                    return Err(OagwError::validation(format!(
                        "field `alias`: the alias is immutable; these endpoints would derive `{derived}` but the upstream is registered as `{stored_alias}` — delete and re-create the upstream"
                    ))
                    .with_alias(stored_alias)
                    .with_invalid_value(derived));
                }
                Ok(stored_alias.to_owned())
            }
            None => {
                if was_derivable {
                    return Err(OagwError::validation(
                        "field `alias`: replacing a derivable endpoint pool with a non-derivable one would change the alias — delete and re-create the upstream",
                    )
                    .with_alias(stored_alias));
                }
                if let Some(supplied) = supplied.map(normalize_alias)
                    && supplied != stored_alias
                {
                    return Err(OagwError::validation(format!(
                        "field `alias`: the alias is immutable and cannot be overridden; the upstream is registered as `{stored_alias}`"
                    ))
                    .with_alias(stored_alias)
                    .with_invalid_value(supplied));
                }
                Ok(stored_alias.to_owned())
            }
        }
    }

    /// Validates the alias pattern of a caller-supplied alias.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the alias does not match the
    /// upstream schema pattern, and when it is the reserved `unmatched` literal
    /// the metrics fold unresolved requests onto.
    pub fn check_alias_shape(&self, alias: &str) -> Result<(), OagwError> {
        if alias == crate::domain::metrics::UNMATCHED_HOST {
            return Err(OagwError::validation(format!(
                "field `alias`: `{}` is a reserved word",
                crate::domain::metrics::UNMATCHED_HOST
            ))
            .with_alias(alias)
            .with_invalid_value(alias));
        }
        if !is_valid_alias(alias) {
            return Err(OagwError::validation(
                "field `alias`: must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$",
            )
            .with_alias(alias)
            .with_invalid_value(alias));
        }
        Ok(())
    }

    /// Validates `cors.allow_credentials` against a wildcard origin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when credentials are allowed while the
    /// origin list contains `*` (the CORS schema's `if/then` rule).
    pub fn validate_cors(&self, cors: &CorsConfig) -> Result<(), OagwError> {
        if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
            return Err(OagwError::validation(
                "field `cors`: `allow_credentials` cannot be combined with the wildcard origin `*`",
            )
            .with_invalid_value("*"));
        }
        Ok(())
    }

    /// Validates a tag list (schema pattern `^[a-z0-9_-]+$`, deduplicated, at
    /// most [`MAX_TAG_COUNT`] entries of at most [`MAX_TAG_LEN`] bytes).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] naming the offending tag.
    pub fn validate_tags(&self, tags: &[String], field: &str) -> Result<(), OagwError> {
        if tags.len() > MAX_TAG_COUNT {
            return Err(OagwError::validation(format!(
                "field `{field}`: at most {MAX_TAG_COUNT} tags are allowed, got {}",
                tags.len()
            ))
            .with_invalid_value(tags.len().to_string()));
        }
        for tag in tags {
            if tag.len() > MAX_TAG_LEN {
                return Err(OagwError::validation(format!(
                    "field `{field}`: a tag must be at most {MAX_TAG_LEN} characters"
                ))
                .with_invalid_value(tag.clone()));
            }
            if !is_valid_tag(tag) {
                return Err(OagwError::validation(format!(
                    "field `{field}`: a tag must match ^[a-z0-9_-]+$"
                ))
                .with_invalid_value(tag.clone()));
            }
        }
        Ok(())
    }

    /// Validates the rate-limit budget.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `sustained.rate` or
    /// `burst.capacity` is below `1`.
    pub fn validate_rate_limit(&self, rate_limit: &RateLimitConfig) -> Result<(), OagwError> {
        if rate_limit.sustained.rate < 1 {
            return Err(OagwError::validation(
                "field `rate_limit.sustained.rate`: must be at least 1",
            )
            .with_invalid_value(rate_limit.sustained.rate.to_string()));
        }
        if let Some(capacity) = rate_limit.burst.as_ref().and_then(|burst| burst.capacity)
            && capacity < 1
        {
            return Err(OagwError::validation(
                "field `rate_limit.burst.capacity`: must be at least 1",
            )
            .with_invalid_value(capacity.to_string()));
        }
        Ok(())
    }

    /// Validates an upstream draft and returns the populated domain model.
    ///
    /// The returned model carries a nil `id`/`tenant_id` and epoch timestamps:
    /// the control-plane service fills those in.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for every rule listed in the module
    /// documentation.
    pub fn validate_upstream(&self, input: &UpstreamInput) -> Result<Upstream, OagwError> {
        let endpoints = &input.server.endpoints;
        self.validate_pool(endpoints)?;
        if let Some(rate_limit) = &input.rate_limit {
            self.validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &input.cors {
            self.validate_cors(cors)?;
        }
        self.validate_tags(&input.tags, "tags")?;
        let alias = self.resolve_create_alias(endpoints, input.alias.as_deref())?;
        Ok(self.build_upstream(input, alias))
    }

    /// Validates an upstream *replace* against the stored resource and returns
    /// the populated model (nil `id`/`tenant_id`).
    ///
    /// The alias is immutable, so it is resolved against the stored pool rather
    /// than derived from scratch; everything else is a full replacement.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for the same rules as
    /// [`Validator::validate_upstream`] plus the alias-immutability rules
    /// documented on [`Validator::resolve_replace_alias`].
    pub fn validate_upstream_replace(
        &self,
        existing: &Upstream,
        input: &UpstreamInput,
    ) -> Result<Upstream, OagwError> {
        let endpoints = &input.server.endpoints;
        self.validate_pool(endpoints)?;
        if let Some(rate_limit) = &input.rate_limit {
            self.validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &input.cors {
            self.validate_cors(cors)?;
        }
        self.validate_tags(&input.tags, "tags")?;
        let alias = self.resolve_replace_alias(
            &existing.alias,
            existing.endpoints(),
            endpoints,
            input.alias.as_deref(),
        )?;
        Ok(self.build_upstream(input, alias))
    }

    /// Assembles the upstream model of a validated draft.
    fn build_upstream(&self, input: &UpstreamInput, alias: String) -> Upstream {
        Upstream {
            enabled: input.enabled.unwrap_or(true),
            alias,
            tags: input.tags.clone(),
            server: ServerConfig {
                endpoints: input.server.endpoints.clone(),
            },
            protocol: input.protocol,
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            ..Upstream::default()
        }
    }

    /// Validates a route draft against its owning upstream and returns the
    /// populated domain model (nil `id`/`tenant_id`, epoch timestamps).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the match block is missing or
    /// ambiguous, when its shape is invalid, when the rate-limit/CORS budgets
    /// are invalid, or when the match protocol differs from the upstream
    /// protocol.
    pub fn validate_route(
        &self,
        upstream_id: Uuid,
        upstream: &Upstream,
        input: &RouteInput,
    ) -> Result<Route, OagwError> {
        match (&input.r#match.http, &input.r#match.grpc) {
            (Some(_), Some(_)) => {
                return Err(OagwError::validation(
                    "field `match`: exactly one of `http` or `grpc` must be present",
                )
                .with_upstream_id(upstream_id));
            }
            (None, None) => {
                return Err(OagwError::validation(
                    "field `match`: one of `http` or `grpc` must be present",
                )
                .with_upstream_id(upstream_id));
            }
            (Some(http), None) => {
                self.validate_http_match(upstream_id, upstream, http)?;
            }
            (None, Some(grpc)) => {
                self.validate_grpc_match(upstream_id, upstream, grpc)?;
            }
        }
        if let Some(rate_limit) = &input.rate_limit {
            self.validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &input.cors {
            self.validate_cors(cors)?;
        }
        self.validate_tags(&input.tags, "tags")?;

        Ok(Route {
            upstream_id,
            r#match: input.r#match.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            enabled: input.enabled.unwrap_or(true),
            priority: input.priority.unwrap_or(0),
            tags: input.tags.clone(),
            ..Route::default()
        })
    }

    /// Validates the `match.http` block of a route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an empty method list, an empty or
    /// non-absolute path (the match is a path *prefix* of the request path,
    /// which always starts with `/`) or a protocol mismatch with the owning
    /// upstream.
    fn validate_http_match(
        &self,
        upstream_id: Uuid,
        upstream: &Upstream,
        http: &crate::domain::model::HttpMatch,
    ) -> Result<(), OagwError> {
        if http.methods.is_empty() {
            return Err(OagwError::validation(
                "field `match.http.methods`: at least one method is required",
            )
            .with_upstream_id(upstream_id));
        }
        if http.path.is_empty() {
            return Err(
                OagwError::validation("field `match.http.path`: must not be empty")
                    .with_upstream_id(upstream_id),
            );
        }
        if !http.path.starts_with('/') {
            return Err(
                OagwError::validation("field `match.http.path`: must start with '/'")
                    .with_path(http.path.as_str())
                    .with_upstream_id(upstream_id),
            );
        }
        if upstream.protocol != Protocol::Http {
            return Err(OagwError::validation(
                "field `match.http`: an HTTP match requires an upstream with the HTTP protocol",
            )
            .with_path(http.path.as_str())
            .with_upstream_id(upstream_id));
        }
        Ok(())
    }

    /// Validates the `match.grpc` block of a route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for empty `service`/`method` fields or
    /// a protocol mismatch with the owning upstream.
    fn validate_grpc_match(
        &self,
        upstream_id: Uuid,
        upstream: &Upstream,
        grpc: &crate::domain::model::GrpcMatch,
    ) -> Result<(), OagwError> {
        if grpc.service.is_empty() || grpc.method.is_empty() {
            return Err(OagwError::validation(
                "field `match.grpc`: `service` and `method` must not be empty",
            )
            .with_upstream_id(upstream_id));
        }
        if upstream.protocol != Protocol::Grpc {
            return Err(OagwError::validation(
                "field `match.grpc`: a gRPC match requires an upstream with the gRPC protocol",
            )
            .with_upstream_id(upstream_id));
        }
        Ok(())
    }

    /// Validates a plugin draft and returns the populated domain model (nil
    /// `id`/`tenant_id`, epoch timestamps).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `plugin_type` is not a GTS type
    /// identifier or when `config` is not a JSON object.
    pub fn validate_plugin(&self, input: &PluginInput) -> Result<Plugin, OagwError> {
        let plugin_type = input.plugin_type.trim();
        if !is_gts_type_id(plugin_type) {
            return Err(OagwError::validation(
                "field `plugin_type`: must be a GTS type identifier such as `gts.cf.core.oagw.guard_plugin.v1`",
            )
            .with_plugin_id(plugin_type)
            .with_invalid_value(plugin_type));
        }
        if !input.config.is_object() {
            return Err(
                OagwError::validation("field `config`: must be a JSON object")
                    .with_plugin_id(plugin_type)
                    .with_invalid_value("non-object configuration"),
            );
        }
        self.validate_tags(&input.tags, "tags")?;

        Ok(Plugin {
            plugin_type: plugin_type.to_owned(),
            config: input.config.clone(),
            enabled: input.enabled.unwrap_or(true),
            tags: input.tags.clone(),
            ..Plugin::default()
        })
    }

    /// Resolves every plugin reference of an upstream draft against `catalog`:
    /// the `auth.type` auth plugin and the `plugins.items[].plugin_ref` chain.
    ///
    /// Both wire spellings of a reference are accepted (see
    /// [`reference_matches_plugin`]): the canonical bare UUID of a custom
    /// plugin, its GTS instance id, and the builtin plugin ids. A reference the
    /// catalog cannot resolve is a **400 `ValidationError`**: the control plane
    /// refuses the draft before it reaches the registry, because an
    /// unresolvable binding would otherwise only surface as a 503
    /// `PluginNotFound` on the data plane, per request.
    ///
    /// The catalog is the calling tenant's own plugin set (see
    /// `ControlPlaneService::plugin_catalog`): ancestor resolution is
    /// asynchronous and is not plumbed into the pure validator, so a descendant
    /// binds only its own custom plugins and the builtins.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] naming the unresolvable reference.
    pub fn validate_bindings(
        &self,
        upstream: &Upstream,
        catalog: &PluginCatalog,
    ) -> Result<(), OagwError> {
        if let Some(auth) = &upstream.auth {
            self.validate_plugin_ref("auth.type", PluginSlot::Auth, &auth.auth_type, catalog)?;
        }
        for binding in &upstream.plugins.items {
            self.validate_plugin_ref(
                "plugins.items[].plugin_ref",
                PluginSlot::Chain,
                &binding.plugin_ref,
                catalog,
            )?;
        }
        Ok(())
    }

    /// Resolves every `plugins.items[].plugin_ref` of a route draft.
    ///
    /// # Errors
    ///
    /// Same rules as [`Validator::validate_bindings`].
    pub fn validate_route_bindings(
        &self,
        route: &Route,
        catalog: &PluginCatalog,
    ) -> Result<(), OagwError> {
        for binding in &route.plugins.items {
            self.validate_plugin_ref(
                "plugins.items[].plugin_ref",
                PluginSlot::Chain,
                &binding.plugin_ref,
                catalog,
            )?;
        }
        Ok(())
    }

    /// Checks one reference against the catalog: it must resolve, and its kind
    /// must fit the slot it is bound in.
    ///
    /// A reference the catalogue cannot place is a `400` naming it; a reference
    /// that resolves but whose base type does not fit [`PluginSlot`] is a `400`
    /// naming the reference *and* the kind the slot expects, because the data
    /// plane would otherwise answer `503 PluginNotFound` for as long as the
    /// binding stayed in place.
    fn validate_plugin_ref(
        &self,
        field: &str,
        slot: PluginSlot,
        reference: &str,
        catalog: &PluginCatalog,
    ) -> Result<(), OagwError> {
        let reference = reference.trim();
        if !catalog.resolves(reference) {
            return Err(OagwError::validation(format!(
                "field `{field}`: plugin `{reference}` is not registered for this tenant"
            ))
            .with_plugin_id(reference)
            .with_invalid_value(reference));
        }
        let Some(kind) = catalog.kind_of(reference) else {
            return Ok(());
        };
        if !slot.accepts().contains(&kind.as_str()) {
            return Err(OagwError::validation(format!(
                "field `{field}`: plugin `{reference}` is a {kind} and cannot be bound in the {} \
                 slot, which requires {}",
                slot.as_str(),
                slot.accepts().to_vec().join(" or ")
            ))
            .with_plugin_id(reference)
            .with_invalid_value(reference));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
