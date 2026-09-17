// Created: 2026-09-04 by Constructor Tech
//! Upstream alias derivation and validation.
//!
//! Implements `cpt-cf-oagw-fr-alias-resolution` (`docs/PRD.md` §5.5):
//!
//! * hostname-based endpoint pools always auto-derive their alias — a
//!   user-provided alias equal to the derived value is accepted as an
//!   idempotent no-op, any other value is rejected;
//! * IP-based or non-derivable endpoint pools require an explicit alias;
//! * a multi-endpoint pool derives the longest common domain suffix
//!   (at least two labels), validated against the public suffix list so a
//!   bare public suffix (`co.uk`) can never become an alias;
//! * aliases are normalized to ASCII lowercase with trailing dots stripped
//!   and resolution is case-insensitive;
//! * aliases are unique per `(tenant_id, alias)`, while a descendant tenant
//!   may *shadow* an ancestor's alias (closest match wins).

use std::fmt;
use std::str::FromStr;

use crate::domain::upstream::Endpoint;
use crate::domain::upstream::EndpointScheme;
use crate::error::OagwError;

/// Maximum alias length (RFC 1035 hostname bound).
pub const MAX_ALIAS_LEN: usize = 253;

/// Minimum number of labels of a multi-endpoint common suffix.
pub const MIN_COMMON_SUFFIX_LABELS: usize = 2;

/// Highest valid port number.
const MAX_PORT: u32 = 65_535;

/// A validated, normalized upstream alias.
///
/// The value is ASCII lowercase, has no trailing dot and satisfies the
/// `alias` pattern of `docs/schemas/upstream.v1.schema.json`
/// (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`), optionally suffixed with
/// `:<port>` for a non-standard port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Alias {
    value: String,
}

impl Alias {
    /// Validates and normalizes a user-supplied alias.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::InvalidAlias`] when the value is empty, too long,
    /// or does not satisfy the alias pattern, and
    /// [`OagwError::AliasShadowsPublicSuffix`] when the value is a bare
    /// public suffix (for example `co.uk`).
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        let normalized = normalize(raw)?;
        if shadows_public_suffix(&normalized) {
            return Err(OagwError::AliasShadowsPublicSuffix { alias: normalized });
        }
        Ok(Self { value: normalized })
    }

    /// Builds an alias from a value that has already been derived from an
    /// endpoint pool. Same validation as [`Alias::parse`].
    ///
    /// # Errors
    ///
    /// See [`Alias::parse`].
    pub fn derived(raw: &str) -> Result<Self, OagwError> {
        Self::parse(raw)
    }

    /// Normalized alias value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Host part of the alias (everything before an optional `:port`).
    #[must_use]
    pub fn host(&self) -> &str {
        self.port_separator()
            .map_or(self.value.as_str(), |(host, _)| host)
    }

    /// Port part of the alias, when the alias carries a non-standard port.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.port_separator()
            .and_then(|(_, port)| port.parse::<u16>().ok())
    }

    /// Splits the alias at its `:port` separator, if any.
    fn port_separator(&self) -> Option<(&str, &str)> {
        let (host, port) = self.value.split_once(':')?;
        Some((host, port))
    }

    /// Builds a tenant-scoped registration key for this alias.
    #[must_use]
    pub fn key_for(&self, tenant_id: uuid::Uuid) -> AliasKey {
        AliasKey::new(tenant_id, self.clone())
    }
}

impl fmt::Display for Alias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

impl FromStr for Alias {
    type Err = OagwError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl AsRef<str> for Alias {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

/// Tenant-scoped uniqueness key of an upstream alias
/// (`docs/PRD.md` §5.5: unique per `(tenant_id, alias)`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AliasKey {
    tenant_id: uuid::Uuid,
    alias: Alias,
}

impl AliasKey {
    /// Builds a key from a tenant and a validated alias.
    #[must_use]
    pub const fn new(tenant_id: uuid::Uuid, alias: Alias) -> Self {
        Self { tenant_id, alias }
    }

    /// Owning tenant.
    #[must_use]
    pub const fn tenant_id(&self) -> uuid::Uuid {
        self.tenant_id
    }

    /// Aliased upstream.
    #[must_use]
    pub const fn alias(&self) -> &Alias {
        &self.alias
    }
}

/// An upstream alias as registered by a tenant: the uniqueness unit of
/// `docs/PRD.md` §5.5, pairing an [`AliasKey`] with the upstream that holds
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRegistration {
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Upstream holding the alias.
    pub upstream_id: uuid::Uuid,
    /// Registered alias.
    pub alias: Alias,
}

impl AliasRegistration {
    /// Builds a registration from its parts.
    #[must_use]
    pub const fn new(tenant_id: uuid::Uuid, upstream_id: uuid::Uuid, alias: Alias) -> Self {
        Self {
            tenant_id,
            upstream_id,
            alias,
        }
    }

    /// Tenant-scoped uniqueness key of this registration.
    #[must_use]
    pub fn key(&self) -> AliasKey {
        AliasKey::new(self.tenant_id, self.alias.clone())
    }
}

/// One level of a tenant hierarchy: a tenant and the aliases it registered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TenantAliasScope {
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Aliases registered by that tenant.
    pub aliases: Vec<Alias>,
}

/// Outcome of deriving an alias from an endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// The pool determines the alias.
    Derived(Alias),
    /// The pool is IP-based or shares no derivable suffix: an explicit alias
    /// is required (`docs/PRD.md` §5.5).
    ExplicitRequired,
}

/// Derives the alias of an endpoint pool (`docs/PRD.md` §5.5).
///
/// * a single hostname yields the hostname (with `:<port>` only when the
///   port is non-standard for its scheme);
/// * multiple hostnames yield the longest common domain suffix with at least
///   [`MIN_COMMON_SUFFIX_LABELS`] labels (again with `:<port>` for a
///   non-standard port);
/// * IP-based endpoints, differing ports or unrelated hostnames require an
///   explicit alias, as does a pool whose only common suffix is a bare public
///   suffix (`foo.co.uk` + `bar.co.uk` → not derivable).
///
/// # Errors
///
/// Returns [`OagwError::InvalidEndpoint`] when the pool is empty and
/// [`OagwError::AliasShadowsPublicSuffix`] when a *single-host* pool would
/// derive a bare public suffix alias such as `co.uk`.
pub fn derive_alias(endpoints: &[Endpoint]) -> Result<AliasDerivation, OagwError> {
    let Some(first) = endpoints.first() else {
        return Err(OagwError::InvalidEndpoint {
            reason: String::from("an alias can only be derived from a non-empty endpoint pool"),
        });
    };
    if endpoints.iter().any(Endpoint::is_ip_endpoint) {
        return Ok(AliasDerivation::ExplicitRequired);
    }
    let port = first.effective_port();
    if endpoints
        .iter()
        .any(|endpoint| endpoint.effective_port() != port)
    {
        return Ok(AliasDerivation::ExplicitRequired);
    }
    let hosts: Vec<&str> = endpoints.iter().map(Endpoint::host).collect();
    let value = match hosts.as_slice() {
        [host] => with_optional_port(host, first.scheme(), port),
        _ => {
            let Some(suffix) = common_suffix(&hosts) else {
                return Ok(AliasDerivation::ExplicitRequired);
            };
            let value = with_optional_port(&suffix, first.scheme(), port);
            // A common suffix that is a bare public suffix (`co.uk`) is not
            // derivable: the pool needs an explicit alias.
            return match Alias::parse(&value) {
                Ok(derived) => Ok(AliasDerivation::Derived(derived)),
                Err(OagwError::AliasShadowsPublicSuffix { .. }) => {
                    Ok(AliasDerivation::ExplicitRequired)
                }
                Err(error) => Err(error),
            };
        }
    };
    let derived = Alias::parse(&value)?;
    Ok(AliasDerivation::Derived(derived))
}

/// Resolves the alias of a new upstream, enforcing the alias contract of
/// `docs/PRD.md` §5.5.
///
/// Hostname-based pools always auto-derive: a requested alias equal to the
/// derived value is accepted as an idempotent no-op, any other value is
/// rejected. IP-based or non-derivable pools require the requested alias.
///
/// # Errors
///
/// Returns [`OagwError::InvalidAlias`] when a requested alias disagrees with
/// the derived one, [`OagwError::AliasRequired`] when an explicit alias is
/// missing, and the [`Alias::parse`] errors otherwise.
pub fn resolve_alias(
    endpoints: &[Endpoint],
    requested: Option<&Alias>,
) -> Result<Alias, OagwError> {
    match derive_alias(endpoints)? {
        AliasDerivation::Derived(derived) => match requested {
            Some(requested) if *requested == derived => Ok(derived),
            Some(requested) => Err(OagwError::InvalidAlias {
                alias: requested.as_str().to_owned(),
                reason: format!(
                    "hostname-based endpoints always auto-derive the alias; expected '{}'",
                    derived.as_str()
                ),
            }),
            None => Ok(derived),
        },
        AliasDerivation::ExplicitRequired => requested.map_or_else(
            || {
                Err(OagwError::AliasRequired {
                    detail: String::from(
                        "IP-based or non-derivable endpoints require an explicit alias",
                    ),
                })
            },
            |requested| Ok(requested.clone()),
        ),
    }
}

/// Rejects an alias that is already registered by the same tenant
/// (`docs/PRD.md` §5.5: unique per `(tenant_id, alias)`).
///
/// Aliases of *other* tenants never collide here: a descendant tenant may
/// register the same alias and shadow the ancestor's upstream (see
/// [`resolve_shadowing`]).
///
/// # Errors
///
/// Returns [`OagwError::AliasConflict`] when `key` is already taken inside
/// its own tenant.
pub fn ensure_alias_unique(key: &AliasKey, taken: &[AliasRegistration]) -> Result<(), OagwError> {
    for existing in taken {
        if existing.tenant_id == key.tenant_id() && existing.alias == *key.alias() {
            return Err(OagwError::AliasConflict {
                alias: key.alias().as_str().to_owned(),
                existing_upstream_id: existing.upstream_id,
            });
        }
    }
    Ok(())
}

/// Resolves an alias across a tenant hierarchy, descendant first
/// (`docs/PRD.md` §5.5 "Shadowing Resolution Order"): the closest match
/// wins, so a descendant upstream shadows an ancestor's.
///
/// `scopes` must be ordered descendant-first; the returned tuple is the
/// owning tenant and the matched alias.
#[must_use]
pub fn resolve_shadowing<'a>(
    alias: &Alias,
    scopes: &'a [TenantAliasScope],
) -> Option<(uuid::Uuid, &'a Alias)> {
    for scope in scopes {
        for registered in &scope.aliases {
            if registered == alias {
                return Some((scope.tenant_id, registered));
            }
        }
    }
    None
}

/// Normalizes and validates an alias value: ASCII lowercase, trailing dots
/// stripped, alias grammar, length bound and a single optional `:<port>`.
fn normalize(raw: &str) -> Result<String, OagwError> {
    let value = raw.trim().to_ascii_lowercase();
    let value = value.trim_end_matches('.');
    if value.is_empty() {
        return Err(OagwError::InvalidAlias {
            alias: raw.to_owned(),
            reason: String::from("the alias must not be empty"),
        });
    }
    if value.len() > MAX_ALIAS_LEN {
        return Err(OagwError::InvalidAlias {
            alias: raw.to_owned(),
            reason: format!("the alias must not exceed {MAX_ALIAS_LEN} characters"),
        });
    }
    if !is_valid_shape(value) {
        return Err(OagwError::InvalidAlias {
            alias: raw.to_owned(),
            reason: String::from("the alias must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"),
        });
    }
    validate_port_suffix(value, raw)?;
    Ok(value.to_owned())
}

/// Checks the `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` alias pattern.
fn is_valid_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    let Some(last) = bytes.last() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-'))
}

/// Rejects a `:<port>` suffix that is not a valid port number.
fn validate_port_suffix(value: &str, raw: &str) -> Result<(), OagwError> {
    let Some((_, port)) = value.split_once(':') else {
        return Ok(());
    };
    if port.contains(':') {
        return Err(OagwError::InvalidAlias {
            alias: raw.to_owned(),
            reason: String::from("the alias may carry at most one ':port' suffix"),
        });
    }
    match port.parse::<u32>() {
        Ok(parsed) if (1..=MAX_PORT).contains(&parsed) => Ok(()),
        _ => Err(OagwError::InvalidAlias {
            alias: raw.to_owned(),
            reason: format!("'{port}' is not a valid port suffix"),
        }),
    }
}

/// `true` when `host` is a bare public suffix (for example `co.uk`) and could
/// therefore shadow a registrable suffix namespace.
fn shadows_public_suffix(host: &str) -> bool {
    if !host.contains('.') {
        // Single-label aliases (`my-internal-service`) are not hostnames and
        // cannot shadow a registrable suffix.
        return false;
    }
    // A registrable domain needs at least one label in front of the public
    // suffix; `None` means the whole input *is* a public suffix.
    psl::domain_str(host).is_none()
}

/// Longest common domain suffix of `hosts`, requiring at least
/// [`MIN_COMMON_SUFFIX_LABELS`] labels.
fn common_suffix(hosts: &[&str]) -> Option<String> {
    let reversed: Vec<Vec<&str>> = hosts
        .iter()
        .map(|host| host.split('.').rev().collect())
        .collect();
    let first = reversed.first()?;
    let mut common: Vec<&str> = Vec::new();
    for (idx, label) in first.iter().enumerate() {
        if reversed
            .iter()
            .all(|labels| labels.get(idx).is_some_and(|candidate| candidate == label))
        {
            common.push(label);
        } else {
            break;
        }
    }
    if common.len() < MIN_COMMON_SUFFIX_LABELS {
        return None;
    }
    common.reverse();
    Some(common.join("."))
}

/// Appends `:<port>` when the port is non-standard for `scheme`.
fn with_optional_port(host: &str, scheme: EndpointScheme, port: u16) -> String {
    if scheme.is_standard_port(port) {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "alias_tests.rs"]
mod alias_tests;
