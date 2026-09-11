//! Plugin identity, classification and the typed plugin contracts.
//!
//! This module is the *domain* half of the plugin system: the three plugin
//! types with their base identifiers and permission strings, the classification
//! of a plugin identifier (`cpt-cf-oagw-algo-plugin-identifier-resolve`) and the
//! plugin trait definitions. Per `cpt-cf-oagw-algo-plugin-catalog-register`
//! (`inst-pcrg-09`) the *registries* live in `infra/plugin/`, so the domain
//! layer holds no registry dependency: it only fixes the shape every registry
//! entry must expose and the names each base type resolves.
//!
//! # Identifier model
//!
//! A plugin identifier is only ever the anonymous GTS form
//! `gts.cf.core.oagw.{type}_plugin.v1~{instance}` where the instance is either
//! the named form `cf.core.oagw.{name}.v1` or a UUID. Two shapes are accepted
//! beside it, each with its own reason:
//!
//! * a bare UUID — accepted as a `{id}` path parameter by inferring the type
//!   from the request path (`inst-pidr-12`), and normalized to the
//!   type-agnostic chain base on an upstream write by entry 2.2;
//! * the type-agnostic base `gts.cf.core.oagw.plugin.v1~` — the reference form
//!   entry 2.2 already stores for a `plugins.items[]` entry, which carries no
//!   plugin type of its own and is therefore classified by its instance alone.

use std::fmt;

use uuid::Uuid;

pub mod resolver;

/// Base identifier of the `auth` plugin type.
pub const AUTH_PLUGIN_BASE: &str = "gts.cf.core.oagw.auth_plugin.v1";

/// Base identifier of the `guard` plugin type.
pub const GUARD_PLUGIN_BASE: &str = "gts.cf.core.oagw.guard_plugin.v1";

/// Base identifier of the `transform` plugin type.
pub const TRANSFORM_PLUGIN_BASE: &str = "gts.cf.core.oagw.transform_plugin.v1";

/// Type-agnostic base identifier entry 2.2 stores for a `plugins.items[]`
/// entry.
///
/// It is not one of the three plugin base types, so it never identifies a
/// definition row and is never accepted as a `{id}` path parameter; it is only
/// classified as a chain reference.
pub const CHAIN_PLUGIN_BASE: &str = "gts.cf.core.oagw.plugin.v1";

/// Instance prefix of every named plugin (`cf.core.oagw.{name}.v1`).
pub const NAMED_INSTANCE_PREFIX: &str = "cf.core.oagw.";

/// Resolvable auth plugin names, in the order the DESIGN lists them.
pub const RESOLVABLE_AUTH: [&str; 4] = [
    "noop",
    "apikey",
    "oauth2_client_cred",
    "oauth2_client_cred_basic",
];

/// Resolvable guard plugin names.
pub const RESOLVABLE_GUARD: [&str; 1] = ["required_headers"];

/// Resolvable transform plugin names.
pub const RESOLVABLE_TRANSFORM: [&str; 1] = ["request_id"];

/// Catalog-only auth plugin names: registered, never resolvable.
pub const CATALOG_ONLY_AUTH: [&str; 2] = ["basic", "bearer"];

/// Catalog-only guard plugin names: registered, never resolvable.
pub const CATALOG_ONLY_GUARD: [&str; 2] = ["timeout", "cors"];

/// Catalog-only transform plugin names: registered, never resolvable.
pub const CATALOG_ONLY_TRANSFORM: [&str; 2] = ["logging", "metrics"];

// @cpt-begin:cpt-cf-oagw-dod-plugin-type-catalog:p1:inst-full
/// One of the three plugin types the gateway declares.
///
/// The type fixes the base identifier, the permission string and the registry
/// the named form resolves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginType {
    /// Credential injection; exactly one per upstream.
    Auth,
    /// Validation and policy enforcement; may reject a request.
    Guard,
    /// Request or response mutation.
    Transform,
}

impl PluginType {
    /// The wire token of the type (`plugin_type`).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// Base identifier of the type (`gts.cf.core.oagw.{type}_plugin.v1`).
    #[must_use]
    pub const fn base_identifier(&self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_BASE,
            Self::Guard => GUARD_PLUGIN_BASE,
            Self::Transform => TRANSFORM_PLUGIN_BASE,
        }
    }

    /// Names the type's builtin registry resolves.
    #[must_use]
    pub const fn resolvable_names(&self) -> &'static [&'static str] {
        match self {
            Self::Auth => &RESOLVABLE_AUTH,
            Self::Guard => &RESOLVABLE_GUARD,
            Self::Transform => &RESOLVABLE_TRANSFORM,
        }
    }

    /// Names the type registers as catalog-only, with no backing
    /// implementation.
    #[must_use]
    pub const fn catalog_only_names(&self) -> &'static [&'static str] {
        match self {
            Self::Auth => &CATALOG_ONLY_AUTH,
            Self::Guard => &CATALOG_ONLY_GUARD,
            Self::Transform => &CATALOG_ONLY_TRANSFORM,
        }
    }

    /// The permission string of an action on the type, exactly as the DESIGN
    /// permission table fixes it.
    #[must_use]
    pub fn permission(&self, action: &str) -> String {
        format!("{}~:{action}", self.base_identifier())
    }

    /// Parse the wire token of a plugin type.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }

    /// The three types, in declaration order.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::Auth, Self::Guard, Self::Transform]
    }
}

impl fmt::Display for PluginType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl serde::Serialize for PluginType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for PluginType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown plugin type `{raw}`")))
    }
}
// @cpt-end:cpt-cf-oagw-dod-plugin-type-catalog:p1:inst-full

/// A pipeline phase a plugin may declare.
///
/// The variants carry the `On` prefix the wire tokens (`on_request`, …) spell,
/// so the rename the lint suggests would unlink them from `Phase::as_str`.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    /// Runs on the request path.
    OnRequest,
    /// Runs on the response path.
    OnResponse,
    /// Runs when the proxy path fails.
    OnError,
}

impl Phase {
    /// The wire token of the phase.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OnRequest => "on_request",
            Self::OnResponse => "on_response",
            Self::OnError => "on_error",
        }
    }

    /// Parse a phase token.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "on_request" => Some(Self::OnRequest),
            "on_response" => Some(Self::OnResponse),
            "on_error" => Some(Self::OnError),
            _ => None,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl serde::Serialize for Phase {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Phase {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("unknown phase `{raw}`")))
    }
}

/// The instance part of a plugin identifier, after `~`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PluginInstance {
    /// A named plugin: `cf.core.oagw.{name}.v1`.
    Named(String),
    /// A UUID-backed custom plugin.
    Uuid(Uuid),
}

/// The base type carried by a plugin identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginBase {
    /// One of the three plugin base types.
    Typed(PluginType),
    /// The type-agnostic chain base of entry 2.2 (`gts...plugin.v1`).
    Chain,
    /// No base at all: a bare UUID, which names no type.
    Bare,
}

/// How a plugin identifier resolves (`cpt-cf-oagw-algo-plugin-identifier-resolve`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginClass {
    /// A UUID-backed custom plugin; the definition row must exist for a
    /// *typed* reference and must carry the matching `plugin_type`.
    Custom(Uuid),
    /// A named plugin resolvable through its base type's registry.
    Named(PluginType, String),
    /// A reserved catalog-only identifier: registered, never resolvable and
    /// never bindable.
    CatalogOnly(PluginType, String),
    /// An instance form that resolves to nothing.
    Unknown,
}

/// A parsed plugin identifier.
///
/// Classification is pure: it needs no store and no registry, because the
/// resolvable names of each base type are fixed by this module and the
/// registries of `infra/plugin/` are built from exactly those names
/// (`inst-pcrg-08`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginIdentifier {
    /// The raw identifier as it arrived.
    pub raw: String,
    /// The base type the identifier carries.
    pub base: PluginBase,
    /// The instance part after `~`.
    pub instance: PluginInstance,
}

impl PluginIdentifier {
    /// Parse a plugin identifier, or `None` when the value is not one.
    ///
    /// Accepted forms:
    ///
    /// * `gts.cf.core.oagw.{type}_plugin.v1~{instance}` — the contract form;
    /// * `gts.cf.core.oagw.plugin.v1~{instance}` — the type-agnostic chain form
    ///   entry 2.2 stores;
    /// * a bare UUID, which carries no base at all (`inst-pidr-12`).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(uuid) = Uuid::parse_str(trimmed) {
            return Some(Self {
                raw: trimmed.to_owned(),
                base: PluginBase::Bare,
                instance: PluginInstance::Uuid(uuid),
            });
        }
        let (base, instance) = trimmed.split_once('~')?;
        if instance.is_empty() {
            return None;
        }
        let base = if base == CHAIN_PLUGIN_BASE {
            PluginBase::Chain
        } else {
            PluginBase::Typed(PluginType::from_base(base)?)
        };
        let instance = match Uuid::parse_str(instance) {
            Ok(uuid) => PluginInstance::Uuid(uuid),
            Err(_) => PluginInstance::Named(instance.to_owned()),
        };
        Some(Self {
            raw: trimmed.to_owned(),
            base,
            instance,
        })
    }

    /// Classify the identifier (`inst-pidr-03` to `inst-pidr-11`).
    #[must_use]
    pub fn classify(&self) -> PluginClass {
        match &self.instance {
            // A UUID-backed reference: the type check against the stored
            // definition is the caller's, because it needs the store.
            PluginInstance::Uuid(uuid) => PluginClass::Custom(*uuid),
            PluginInstance::Named(name) => self.classify_named(name),
        }
    }

    /// Classify a named instance against the base type's name sets.
    fn classify_named(&self, instance: &str) -> PluginClass {
        // The registry key is the bare name; the instance is the namespaced
        // form `cf.core.oagw.{name}.v1` (`inst-pcrg-08`).
        let Some(name) = named_name(instance) else {
            return PluginClass::Unknown;
        };
        let candidates: &[PluginType] = match self.base {
            PluginBase::Typed(plugin_type) => &[plugin_type],
            // A type-agnostic chain reference names a plugin without naming its
            // type, so every registry is consulted.
            PluginBase::Chain | PluginBase::Bare => &PluginType::all(),
        };
        for plugin_type in candidates {
            if plugin_type.catalog_only_names().contains(&name) {
                return PluginClass::CatalogOnly(*plugin_type, name.to_owned());
            }
        }
        for plugin_type in candidates {
            if plugin_type.resolvable_names().contains(&name) {
                return PluginClass::Named(*plugin_type, name.to_owned());
            }
        }
        PluginClass::Unknown
    }

    /// Whether the identifier carries a base type at all.
    #[must_use]
    pub const fn is_bare_uuid(&self) -> bool {
        matches!(self.base, PluginBase::Bare)
    }

    /// The full GTS identifier of a definition of this identifier's type and
    /// instance.
    ///
    /// A bare UUID and a type-agnostic chain reference have no type of their
    /// own, so the caller supplies it.
    #[must_use]
    pub fn definition_id(&self, plugin_type: PluginType) -> String {
        match &self.instance {
            PluginInstance::Named(name) => {
                format!("{}~{name}", plugin_type.base_identifier())
            }
            PluginInstance::Uuid(uuid) => format!("{}~{uuid}", plugin_type.base_identifier()),
        }
    }
}

impl PluginType {
    /// The plugin type of a base identifier, or `None` for another base.
    #[must_use]
    pub fn from_base(base: &str) -> Option<Self> {
        match base {
            AUTH_PLUGIN_BASE => Some(Self::Auth),
            GUARD_PLUGIN_BASE => Some(Self::Guard),
            TRANSFORM_PLUGIN_BASE => Some(Self::Transform),
            _ => None,
        }
    }
}

/// The registry key of a named instance, or `None` for another shape.
///
/// A named instance is exactly `cf.core.oagw.{name}.v1`, so the key is the name
/// the builtin registries are keyed by (`inst-pcrg-08`).
#[must_use]
pub fn named_name(instance: &str) -> Option<&str> {
    let rest = instance.strip_prefix(NAMED_INSTANCE_PREFIX)?;
    let name = rest.strip_suffix(".v1")?;
    let well_formed = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    well_formed.then_some(name)
}

/// The UUID instance part of a plugin definition identifier, or `None`.
///
/// A definition is always UUID-backed, so a named instance has no definition
/// row and resolves as missing.
#[must_use]
pub fn definition_uuid(identifier: &str) -> Option<Uuid> {
    match PluginIdentifier::parse(identifier)?.instance {
        PluginInstance::Uuid(uuid) => Some(uuid),
        PluginInstance::Named(_) => None,
    }
}

// @cpt-begin:cpt-cf-oagw-dod-builtin-registries:p1:inst-full
///
/// ADR 0002 fixes the three plugin types and their execution order; entry 2.5
/// owns the resolution of a registry entry into an executable instance and the
/// execution itself (`inst-pcat-11`), so a declaration here exposes identity and
/// declared phases only: no interpreter, no network, no sandbox.
pub trait PluginDeclaration: Send + Sync {
    /// The full GTS identifier of the plugin.
    fn identifier(&self) -> &str;
    /// The plugin type.
    fn plugin_type(&self) -> PluginType;
    /// The named instance the registry keys the plugin by.
    fn name(&self) -> &str;
    /// The phases the plugin declares.
    fn phases(&self) -> &'static [Phase];
}

/// Credential injection plugin; exactly one per upstream.
pub trait AuthPlugin: PluginDeclaration {}

/// Validation and policy enforcement plugin; may reject a request.
pub trait GuardPlugin: PluginDeclaration {}

/// Request or response mutation plugin.
pub trait TransformPlugin: PluginDeclaration {}
// @cpt-end:cpt-cf-oagw-dod-builtin-registries:p1:inst-full
