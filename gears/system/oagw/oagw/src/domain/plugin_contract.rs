//! The three plugin contracts, one registry per contract, and the catalogue
//! distinctions a resolution answers with.
//!
//! Realizes `cpt-cf-oagw-algo-plugin-contract-registry` and
//! `cpt-cf-oagw-dod-plugin-contracts-registries`: [`AuthPlugin`],
//! [`GuardPlugin`], and [`TransformPlugin`] are declared here with one
//! registry each — [`AuthPluginRegistry`], [`GuardPluginRegistry`],
//! [`TransformPluginRegistry`] — and the three registries are kept separate by
//! construction, because each holds its own table and accepts only its own
//! trait object, so an auth identifier is never looked up in the guard or
//! transform registry and a guard identifier never in the transform one.
//!
//! The [`AuthContext`], [`RequestContext`], and [`ResponseContext`] parameters
//! are the foundation's shared vocabulary, declared in
//! [`crate::domain::context`]; the fourth, `ErrorContext`, is the foundation's
//! [`crate::domain::error::ErrorContext`]. None of the four is redeclared
//! here.
//!
//! The sandbox limits of `cpt-cf-oagw-nfr-starlark-sandbox` are part of this
//! surface as [`SANDBOX_LIMITS`], and none of them is enforced here: their
//! enforcement is execution-time work that belongs to the data-plane proxy
//! feature. A custom plugin's Starlark source is therefore never parsed,
//! compiled, sandbox-checked, or executed by anything in this module.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use toolkit_macros::domain_model;

use crate::domain::context::{AuthContext, RequestContext, ResponseContext};
use crate::domain::error::ErrorContext;
use crate::gts::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};

/// The sandbox limits the contract surface exposes, and enforces none of.
pub const SANDBOX_LIMITS: SandboxLimits = SandboxLimits {
    network_io: false,
    file_io: false,
    imports: false,
    max_invocation_millis: 100,
    max_invocation_memory_bytes: 10 * 1024 * 1024,
};

/// The limits a Starlark plugin invocation is held to at execution time.
///
/// Every member is a ceiling the data-plane proxy applies when it runs a
/// plugin; this module publishes them so a caller can read what an invocation
/// is entitled to and so the two features cannot disagree about the numbers.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    /// Whether an invocation may open a network connection.
    pub network_io: bool,
    /// Whether an invocation may touch the file system.
    pub file_io: bool,
    /// Whether an invocation may import a module.
    pub imports: bool,
    /// Wall-clock ceiling of one invocation, in milliseconds.
    pub max_invocation_millis: u64,
    /// Memory ceiling of one invocation, in bytes.
    pub max_invocation_memory_bytes: usize,
}

/// One of the three plugin families a plugin belongs to.
///
/// The family is what the `plugin_type` literal names, what the anonymous GTS
/// identifier's base type names, and what the binding slot carries; all three
/// spellings agree, and [`PluginFamily::from_type_literal`] and
/// [`PluginFamily::parse_identifier`] are the two places the agreement is
/// established.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginFamily {
    /// The authentication plugin family: one per upstream, never on a route.
    Auth,
    /// The guard plugin family: many per upstream and per route.
    Guard,
    /// The transform plugin family: many per upstream and per route.
    Transform,
}

impl PluginFamily {
    /// The three literals `plugin_type` admits, in catalogue order.
    pub const LITERALS: [&str; 3] = ["auth", "guard", "transform"];

    /// The family one `plugin_type` literal names, or `None` for any other
    /// spelling.
    #[must_use]
    pub fn from_type_literal(literal: &str) -> Option<Self> {
        match literal {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }

    /// The lowercase singular name the family is written as on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The base type schema the family's anonymous GTS identifiers are
    /// instances of.
    #[must_use]
    pub const fn base_type(self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_TYPE,
            Self::Guard => GUARD_PLUGIN_TYPE,
            Self::Transform => TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// The phases the family's contract exposes.
    #[must_use]
    pub const fn supported_phases(self) -> &'static [PluginPhase] {
        match self {
            Self::Auth => &[PluginPhase::Auth],
            Self::Guard => &[PluginPhase::GuardRequest, PluginPhase::GuardResponse],
            Self::Transform => &[
                PluginPhase::TransformRequest,
                PluginPhase::TransformResponse,
                PluginPhase::TransformError,
            ],
        }
    }

    /// Parses an anonymous GTS identifier into its family and the instance
    /// part after the `~` separator.
    ///
    /// A bare UUID names a custom plugin row and is answered with the UUID and
    /// no family: the row's own `plugin_type` names the family, not the
    /// identifier. An identifier that is not a plugin identifier at all is
    /// answered `None`.
    #[must_use]
    pub fn parse_identifier(identifier: &str) -> Option<(Option<Self>, &str)> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-parse
        let Some(instance) = identifier
            .strip_prefix(AUTH_PLUGIN_TYPE)
            .map(|tail| (Self::Auth, tail))
            .or_else(|| {
                identifier
                    .strip_prefix(GUARD_PLUGIN_TYPE)
                    .map(|tail| (Self::Guard, tail))
            })
            .or_else(|| {
                identifier
                    .strip_prefix(TRANSFORM_PLUGIN_TYPE)
                    .map(|tail| (Self::Transform, tail))
            })
        else {
            // No plugin base type prefixes it: the only bare form a binding
            // may name is the custom plugin's UUID.
            let is_uuid = uuid::Uuid::parse_str(identifier).is_ok();
            return is_uuid.then_some((None, identifier));
        };
        let (family, instance) = instance;
        let is_named = !instance.is_empty() && !instance.contains('~');
        is_named.then_some((Some(family), instance))
        // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-parse
    }

    /// Whether one identifier is the anonymous GTS identifier of this family.
    #[must_use]
    pub fn names(self, identifier: &str) -> bool {
        identifier.starts_with(self.base_type())
            && identifier.len() > self.base_type().len()
    }
}

/// One phase a plugin implementation may declare.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginPhase {
    /// Credential injection, before the guards run.
    Auth,
    /// Guard evaluation before the upstream call.
    GuardRequest,
    /// Guard evaluation after the upstream call.
    GuardResponse,
    /// Request transformation before the upstream call.
    TransformRequest,
    /// Response transformation after the upstream call.
    TransformResponse,
    /// Error transformation when the upstream call fails.
    TransformError,
}

/// Why an identifier could not be resolved to an implementation.
///
/// The two catalogue reasons are deliberately distinct so a caller can tell a
/// reserved-but-unimplemented identifier from a typo, and neither carries
/// anything about the registry's contents beyond the fact of the failure.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginResolveError {
    /// The identifier is one of the six catalog-only identifiers: registered
    /// in the types-registry only, with no backing implementation anywhere.
    Reserved {
        /// The identifier that was resolved.
        identifier: String,
    },
    /// The identifier is neither registered nor reserved: no catalogue row
    /// names it and no registry holds it.
    Unknown {
        /// The identifier that was resolved.
        identifier: String,
    },
    /// The implementation is registered but does not declare the phase that
    /// was asked for.
    PhaseNotDeclared {
        /// The identifier that was resolved.
        identifier: String,
        /// The phase the implementation does not declare.
        phase: PluginPhase,
    },
}

/// The credential-injection contract.
///
/// One implementation per upstream, never on a route. The single phase it
/// exposes is the credential injection that runs before the guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Whether the implementation declares one phase.
    fn declares(&self, phase: PluginPhase) -> bool;

    /// Resolves the credential the configuration names and writes it into the
    /// context. The material leaves this call in the context's headers and
    /// nowhere else.
    ///
    /// # Errors
    ///
    /// Returns the typed failure the caller maps onto the catalogue, without
    /// echoing a reference value or any material.
    async fn authenticate(
        &self,
        ctx: &mut AuthContext,
        config: &Value,
    ) -> Result<(), PluginFailure>;
}

/// The request-and-response evaluation contract.
///
/// Many implementations per upstream and per route. Both phases are exposed by
/// every guard implementation; one that evaluates only one of them answers
/// `false` for the other in `declares`.
pub trait GuardPlugin: Send + Sync {
    /// Whether the implementation declares one phase.
    fn declares(&self, phase: PluginPhase) -> bool;

    /// Evaluates the request before the upstream call.
    fn guard_request(&self, ctx: &RequestContext, config: &Value) -> GuardDecision;

    /// Evaluates the response after the upstream call.
    fn guard_response(&self, ctx: &ResponseContext, config: &Value) -> GuardDecision;
}

/// The request, response, and error mutation contract.
///
/// Many implementations per upstream and per route. The three phases are the
/// whole surface, and one that mutates only the request answers `false` for
/// the other two in `declares`.
pub trait TransformPlugin: Send + Sync {
    /// Whether the implementation declares one phase.
    fn declares(&self, phase: PluginPhase) -> bool;

    /// Mutates the request before the upstream call.
    fn transform_request(&self, ctx: &mut RequestContext, config: &Value);

    /// Mutates the response after the upstream call.
    fn transform_response(&self, ctx: &mut ResponseContext, config: &Value);

    /// Mutates the error context when the upstream call fails.
    fn transform_error(&self, ctx: &mut ErrorContext, config: &Value);
}

/// The allow-or-reject verdict a guard plugin returns.
///
/// A rejection carries the machine-readable code ADR 0009 names and the
/// message the answer carries; the HTTP status and the catalogue row are
/// decided by the phase the rejection happened in, not by the guard.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The request or the response passes the configured contract.
    Allow,
    /// The request or the response violates it.
    Reject {
        /// Machine-readable rejection code, `REQUIRED_HEADER_MISSING` for the
        /// required-headers guard.
        code: String,
        /// Human-readable message naming the violated property.
        message: String,
    },
}

impl GuardDecision {
    /// Builds a rejection with its code and message.
    #[must_use]
    pub fn reject(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Reject {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Whether the verdict allows what was evaluated.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// The typed failure a plugin returns, before any mapping onto the catalogue.
///
/// No variant carries a credential reference value or resolved material: the
/// reason a failure names is a property of the configuration, never a copy of
/// one.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginFailure {
    /// A credential reference in the configuration failed the `cred://` shape
    /// check. Never reached the credential store.
    CredentialShape,
    /// The credential store resolved no secret for a reference.
    SecretNotFound,
    /// The credential store declined a reference for the calling tenant or
    /// subject, or the identity provider refused the exchange.
    AuthenticationFailed,
    /// The credential store or the identity provider was unreachable.
    Unavailable,
    /// The plugin configuration is unusable for the phase that ran.
    Configuration {
        /// Which property of the configuration is unusable.
        reason: String,
    },
}
/// The registry the `AuthPlugin` implementations are held in.
///
/// The two Client Credentials variants are constructed with the token cache
/// and the credential store they resolve through, which is why the built-in
/// set is built through `AuthPluginRegistry::with_builtins` rather than
/// through `register`.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    /// The registered implementations, keyed on the full identifier.
    entries: BTreeMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the registry with the four backed auth implementations of the
    /// built-in catalogue registered at initialization.
    ///
    /// The `apikey` variant resolves its reference through the credential
    /// store it is handed; the two Client Credentials variants resolve their
    /// two references through the same store and hold the token cache the
    /// ceilings of `token_cache` give them. The `noop` variant holds nothing.
    #[must_use]
    pub fn with_builtins(
        store: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_cache: crate::plugins::token_cache::TokenCacheConfig,
    ) -> Self {
        // @cpt-begin:cpt-cf-oagw-dod-builtin-catalogue:p1:inst-catalog-auth-registry
        let mut registry = Self::default();
        // One cache per variant: the cache key carries the auth method tag, so
        // two variants never share an entry, and each cache is its own
        // instance as ADR 0008's plugin sketch builds them.
        let cache = || crate::plugins::token_cache::TokenCache::new(token_cache);
        for (identifier, plugin) in [
            (
                crate::gts::plugin_catalog::AUTH_NOOP,
                Arc::new(crate::plugins::builtin::NoopAuthPlugin)
                    as Arc<dyn AuthPlugin>,
            ),
            (
                crate::gts::plugin_catalog::AUTH_APIKEY,
                Arc::new(crate::plugins::builtin::ApiKeyAuthPlugin::new(Arc::clone(&store)))
                    as Arc<dyn AuthPlugin>,
            ),
            (
                crate::gts::plugin_catalog::AUTH_OAUTH2_CLIENT_CRED,
                Arc::new(crate::plugins::builtin::OAuth2ClientCredAuthPlugin::new(
                    Arc::clone(&store),
                    toolkit_auth::ClientAuthMethod::Form,
                    cache(),
                )) as Arc<dyn AuthPlugin>,
            ),
            (
                crate::gts::plugin_catalog::AUTH_OAUTH2_CLIENT_CRED_BASIC,
                Arc::new(crate::plugins::builtin::OAuth2ClientCredAuthPlugin::new(
                    Arc::clone(&store),
                    toolkit_auth::ClientAuthMethod::Basic,
                    cache(),
                )) as Arc<dyn AuthPlugin>,
            ),
        ] {
            registry.register(identifier, plugin);
        }
        registry
        // @cpt-end:cpt-cf-oagw-dod-builtin-catalogue:p1:inst-catalog-auth-registry
    }

    /// Registers one implementation under its full anonymous GTS identifier.
    /// A re-registration over an existing identifier replaces it, which is
    /// what a re-registration over byte-identical content amounts to.
    pub fn register(&mut self, identifier: &str, plugin: Arc<dyn AuthPlugin>) {
        self.entries.insert(String::from(identifier), plugin);
    }

    /// The identifiers the registry holds, in sorted order.
    #[must_use]
    pub fn identifiers(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The number of registered implementations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no implementation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolves one identifier, answering the catalogue distinctions before
    /// the registry is consulted at all.
    ///
    /// # Errors
    ///
    /// Returns [`PluginResolveError::Reserved`] for a catalog-only identifier
    /// and [`PluginResolveError::Unknown`] for every identifier no entry of
    /// this registry backs, whether the catalogue names it for another family
    /// or names it not at all.
    pub fn resolve(&self, identifier: &str) -> Result<Arc<dyn AuthPlugin>, PluginResolveError> {
        resolve_entry(&self.entries, identifier, |plugin| Arc::clone(plugin))
    }

    /// Resolves one identifier for one phase, refusing an implementation that
    /// does not declare the phase asked for.
    ///
    /// # Errors
    ///
    /// Returns the same refusals as [`Self::resolve`], plus
    /// [`PluginResolveError::PhaseNotDeclared`] when the resolved
    /// implementation does not declare the phase.
    pub fn resolve_for_phase(
        &self,
        identifier: &str,
        phase: PluginPhase,
    ) -> Result<Arc<dyn AuthPlugin>, PluginResolveError> {
        let plugin = self.resolve(identifier)?;
        if plugin.declares(phase) {
            Ok(plugin)
        } else {
            Err(PluginResolveError::PhaseNotDeclared {
                identifier: String::from(identifier),
                phase,
            })
        }
    }

    /// The phases the identifier's implementation declares, in the family's
    /// supported order, or `None` when the identifier does not resolve.
    #[must_use]
    pub fn declared_phases(&self, identifier: &str) -> Option<Vec<PluginPhase>> {
        let plugin = self.resolve(identifier).ok()?;
        Some(declared_phases(
            |phase| plugin.declares(phase),
            PluginFamily::Auth,
        ))
    }
}

/// The registry the `GuardPlugin` implementations are held in.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    /// The registered implementations, keyed on the full identifier.
    entries: BTreeMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the registry with the one backed guard implementation of the
    /// built-in catalogue registered at initialization.
    ///
    /// The catalogue's other two guard identifiers are core data-plane
    /// behaviour rather than guard implementations, so no second entry exists
    /// to register.
    #[must_use]
    pub fn with_builtins() -> Self {
        // @cpt-begin:cpt-cf-oagw-dod-builtin-catalogue:p1:inst-catalog-guard-registry
        let mut registry = Self::default();
        registry.register(
            crate::gts::plugin_catalog::GUARD_REQUIRED_HEADERS,
            Arc::new(crate::plugins::builtin::RequiredHeadersGuardPlugin),
        );
        registry
        // @cpt-end:cpt-cf-oagw-dod-builtin-catalogue:p1:inst-catalog-guard-registry
    }

    /// Registers one implementation under its full anonymous GTS identifier.
    pub fn register(&mut self, identifier: &str, plugin: Arc<dyn GuardPlugin>) {
        self.entries.insert(String::from(identifier), plugin);
    }

    /// The identifiers the registry holds, in sorted order.
    #[must_use]
    pub fn identifiers(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The number of registered implementations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no implementation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolves one identifier, answering the catalogue distinctions before
    /// the registry is consulted at all.
    ///
    /// # Errors
    ///
    /// Returns [`PluginResolveError::Reserved`] for a catalog-only identifier
    /// and [`PluginResolveError::Unknown`] for every identifier no entry of
    /// this registry backs, whether the catalogue names it for another family
    /// or names it not at all.
    pub fn resolve(&self, identifier: &str) -> Result<Arc<dyn GuardPlugin>, PluginResolveError> {
        resolve_entry(&self.entries, identifier, |plugin| Arc::clone(plugin))
    }

    /// Resolves one identifier for one phase, refusing an implementation that
    /// does not declare the phase asked for.
    ///
    /// # Errors
    ///
    /// Returns the same refusals as [`Self::resolve`], plus
    /// [`PluginResolveError::PhaseNotDeclared`].
    pub fn resolve_for_phase(
        &self,
        identifier: &str,
        phase: PluginPhase,
    ) -> Result<Arc<dyn GuardPlugin>, PluginResolveError> {
        let plugin = self.resolve(identifier)?;
        if plugin.declares(phase) {
            Ok(plugin)
        } else {
            Err(PluginResolveError::PhaseNotDeclared {
                identifier: String::from(identifier),
                phase,
            })
        }
    }

    /// The phases the identifier's implementation declares, or `None` when the
    /// identifier does not resolve.
    #[must_use]
    pub fn declared_phases(&self, identifier: &str) -> Option<Vec<PluginPhase>> {
        let plugin = self.resolve(identifier).ok()?;
        Some(declared_phases(
            |phase| plugin.declares(phase),
            PluginFamily::Guard,
        ))
    }
}

/// The registry the `TransformPlugin` implementations are held in.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    /// The registered implementations, keyed on the full identifier.
    entries: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the registry with the one backed transform implementation of
    /// the built-in catalogue registered at initialization.
    ///
    /// The catalogue's other two transform identifiers are core data-plane
    /// instrumentation rather than transform implementations, so no second
    /// entry exists to register.
    #[must_use]
    pub fn with_builtins() -> Self {
        // @cpt-begin:cpt-cf-oagw-dod-builtin-catalogue:p1:inst-catalog-transform-registry
        let mut registry = Self::default();
        registry.register(
            crate::gts::plugin_catalog::TRANSFORM_REQUEST_ID,
            Arc::new(crate::plugins::builtin::RequestIdTransformPlugin),
        );
        registry
        // @cpt-end:cpt-cf-oagw-dod-builtin-catalogue:p1:inst-catalog-transform-registry
    }

    /// Registers one implementation under its full anonymous GTS identifier.
    pub fn register(&mut self, identifier: &str, plugin: Arc<dyn TransformPlugin>) {
        self.entries.insert(String::from(identifier), plugin);
    }

    /// The identifiers the registry holds, in sorted order.
    #[must_use]
    pub fn identifiers(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The number of registered implementations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no implementation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolves one identifier, answering the catalogue distinctions before
    /// the registry is consulted at all.
    ///
    /// # Errors
    ///
    /// Returns [`PluginResolveError::Reserved`] for a catalog-only identifier
    /// and [`PluginResolveError::Unknown`] for every identifier no entry of
    /// this registry backs, whether the catalogue names it for another family
    /// or names it not at all.
    pub fn resolve(
        &self,
        identifier: &str,
    ) -> Result<Arc<dyn TransformPlugin>, PluginResolveError> {
        resolve_entry(&self.entries, identifier, |plugin| Arc::clone(plugin))
    }

    /// Resolves one identifier for one phase, refusing an implementation that
    /// does not declare the phase asked for.
    ///
    /// # Errors
    ///
    /// Returns the same refusals as [`Self::resolve`], plus
    /// [`PluginResolveError::PhaseNotDeclared`].
    pub fn resolve_for_phase(
        &self,
        identifier: &str,
        phase: PluginPhase,
    ) -> Result<Arc<dyn TransformPlugin>, PluginResolveError> {
        let plugin = self.resolve(identifier)?;
        if plugin.declares(phase) {
            Ok(plugin)
        } else {
            Err(PluginResolveError::PhaseNotDeclared {
                identifier: String::from(identifier),
                phase,
            })
        }
    }

    /// The phases the identifier's implementation declares, or `None` when the
    /// identifier does not resolve.
    #[must_use]
    pub fn declared_phases(&self, identifier: &str) -> Option<Vec<PluginPhase>> {
        let plugin = self.resolve(identifier).ok()?;
        Some(declared_phases(
            |phase| plugin.declares(phase),
            PluginFamily::Transform,
        ))
    }
}

/// The lookup one registry performs: the catalogue distinctions first, then
/// the registry's own table.
fn resolve_entry<T, F>(entries: &BTreeMap<String, T>, identifier: &str, clone: F) -> Result<T, PluginResolveError>
where
    F: FnOnce(&T) -> T,
{
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-catalog-if
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-catalog-return
    // The catalogue table is consulted before the registry, so a reserved
    // identifier is never mistaken for an unknown one.
    if crate::gts::plugin_catalog::is_catalog_only(identifier) {
        return Err(PluginResolveError::Reserved {
            identifier: String::from(identifier),
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-catalog-return
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-catalog-if
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-else
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-lookup
    let found = entries.get(identifier);
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-lookup
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-empty-if
    // A registry answers `Unknown` for every identifier it does not hold,
    // including a backed identifier of another family: the separation the
    // three registries enforce is that an auth identifier is never
    // resolvable from the guard or transform registry.
    let Some(entry) = found else {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-empty-return
        return Err(PluginResolveError::Unknown {
            identifier: String::from(identifier),
        });
        // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-empty-return
    };
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-empty-if
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-empty-else
    // @cpt-begin:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-return
    Ok(clone(entry))
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-return
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-empty-else
    // @cpt-end:cpt-cf-oagw-algo-plugin-contract-registry:p1:inst-reg-else
}

/// The phases one resolved implementation declares, in the family's order.
///
/// The declaration predicate is passed rather than the implementation, because
/// the three contracts are three distinct traits and no shared supertrait
/// names `declares`.
fn declared_phases(
    declares: impl Fn(PluginPhase) -> bool,
    family: PluginFamily,
) -> Vec<PluginPhase> {
    family
        .supported_phases()
        .iter()
        .copied()
        .filter(|phase| declares(*phase))
        .collect()
}

/// What the management surface needs to know about one named plugin: the
/// family its base type names and the phases its implementation declares.
///
/// No implementation object is carried: a binding write asks only whether an
/// identifier resolves and what it declares, and never invokes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedIdentity {
    /// The family the identifier's base type names.
    pub family: PluginFamily,
    /// The phases the implementation declares, in the family's order.
    pub phases: Vec<PluginPhase>,
}

/// The registry the management surface resolves named plugin identifiers
/// through.
///
/// The three implementation registries answer `authenticate`, `guard_*`, and
/// `transform_*`; this one answers the question a binding write asks — is
/// this named identifier backed by an implementation, and which phases does
/// that implementation declare. It is built from the same built-in entries the
/// implementation registries register, with the phases each declares, and it
/// holds no implementation, so a management write that resolves identifiers
/// gains no credential-store dependency and cannot invoke a plugin.
#[derive(Clone, Default)]
pub struct NamedPluginRegistry {
    /// The named identities, keyed on the full identifier.
    entries: BTreeMap<String, NamedIdentity>,
}

impl NamedPluginRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the registry with the built-in identities of the catalogue: the
    /// guard and transform entries are read off the implementations those two
    /// registries register, and the auth entries are the four backed auth
    /// identifiers, each declaring the single credential-injection phase the
    /// auth contract exposes.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        let guard = GuardPluginRegistry::with_builtins();
        for identifier in guard.identifiers() {
            let phases = guard.declared_phases(identifier).unwrap_or_default();
            registry.record(identifier, PluginFamily::Guard, phases);
        }
        let transform = TransformPluginRegistry::with_builtins();
        for identifier in transform.identifiers() {
            let phases = transform.declared_phases(identifier).unwrap_or_default();
            registry.record(identifier, PluginFamily::Transform, phases);
        }
        for (identifier, family) in crate::gts::plugin_catalog::BACKED
            .iter()
            .filter(|(_, family)| *family == PluginFamily::Auth)
        {
            registry.record(
                identifier,
                *family,
                family.supported_phases().to_vec(),
            );
        }
        registry
    }

    /// Records one named identity under its full identifier.
    pub fn record(&mut self, identifier: &str, family: PluginFamily, phases: Vec<PluginPhase>) {
        self.entries.insert(
            String::from(identifier),
            NamedIdentity { family, phases },
        );
    }

    /// The identifiers the registry holds, in sorted order.
    #[must_use]
    pub fn identifiers(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Resolves one named identifier to the identity the implementation
    /// registry holds for it.
    ///
    /// The lookup is the same one the implementation registries perform: the
    /// catalogue distinctions first, then the registry's own table, so a
    /// catalog-only identifier answers `Reserved` and an identifier no
    /// implementation backs answers `Unknown` — including a backed identifier
    /// of another family.
    ///
    /// # Errors
    ///
    /// Returns the same refusals [`GuardPluginRegistry::resolve`] does.
    pub fn resolve(&self, identifier: &str) -> Result<NamedIdentity, PluginResolveError> {
        resolve_entry(&self.entries, identifier, |entry| entry.clone())
    }
}
