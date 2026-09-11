//! The plugin chain the request-phase and response-phase hook points execute
//! (`cpt-cf-oagw-algo-plugin-chain`).
//!
//! The chain takes the merged configuration the pipeline hands the hook — the
//! effective `auth` declaration, the ordered binding list, the effective `cors`
//! configuration and the effective rate-limit bound — and resolves every
//! binding through the three registries entry 2.3 built. Nothing is
//! re-validated, re-ordered or re-written and no definition is mutated
//! (`inst-alc-01`): the chain resolves, executes and stops at the first
//! rejection.
//!
//! The stages run in the order the algorithm fixes: the auth plugin, then the
//! guards — the RequiredHeaders decision, the rate limit and the CORS
//! enforcement — then the request transforms, then the upstream call the
//! pipeline owns, then the response and error transforms (`inst-alc-07`). Each
//! stage is the merged binding list of one plugin type in binding-position
//! order, upstream bindings before route bindings, which is the order the merge
//! already produced (`inst-alc-08`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};
use toolkit_auth::oauth2::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use toolkit_http::{HttpClientConfig, TransportSecurity};
use url::Url;

use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::model::{PluginBinding, RateScope};
use crate::domain::plugin::{PluginClass, PluginIdentifier, PluginType};
use crate::infra::plugin::PluginCatalogError;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::context::{
    PluginOutcome, RateLimitObservation, RequestContext, ResponseContext, REQUIRED_HEADER_MISSING,
};
use crate::infra::proxy::credentials::CredentialSource;
use crate::infra::proxy::effective::EffectiveUpstream;
use crate::infra::proxy::hooks::{PluginChains, PluginRequest, RequestEffects};
use crate::infra::proxy::rate_limit::{self, RateDecision, RateLimiter, RateOutcome, RatePlan};
use crate::infra::proxy::token_cache::{self, TokenCache};
use crate::infra::proxy::validate::{self, CorsDecision};

/// The plugin name of the no-op auth plugin.
const NOOP_PLUGIN: &str = "noop";
/// The plugin name of the API key auth plugin.
const APIKEY_PLUGIN: &str = "apikey";
/// The plugin name of the client-credentials Form variant.
const OAUTH2_FORM_PLUGIN: &str = "oauth2_client_cred";
/// The plugin name of the client-credentials Basic variant.
const OAUTH2_BASIC_PLUGIN: &str = "oauth2_client_cred_basic";
/// The plugin name of the RequiredHeaders guard.
const REQUIRED_HEADERS_PLUGIN: &str = "required_headers";
/// The plugin name of the RequestId transform.
const REQUEST_ID_PLUGIN: &str = "request_id";

/// The auth-method tag the no-op plugin records.
const TAG_NOOP: &str = "noop";
/// The auth-method tag the API key plugin records.
const TAG_API_KEY: &str = "api_key";
/// The auth-method tag the Form variant records.
const TAG_FORM: &str = "form";
/// The auth-method tag the Basic variant records.
const TAG_BASIC: &str = "basic";

/// The `Authorization` scheme the client-credentials plugins inject.
const BEARER: &str = "Bearer";
/// The header the API key plugin injects into when the config names none, in
/// the lowercase form a `HeaderName` is built from.
const DEFAULT_API_KEY_HEADER: &str = "x-api-key";
/// The query member the API key plugin injects into (`inst-ai-08`).
const API_KEY_QUERY: &str = "api_key";
/// The config key of the API key plugin's credential reference.
const API_KEY_REF: &str = "secret_ref";
/// The config key naming the API key plugin's injection target.
const API_KEY_TARGET: &str = "target";
/// The `target` value that selects query injection.
const TARGET_QUERY: &str = "query";
/// The config key of the API key plugin's header name.
const API_KEY_HEADER: &str = "header";
/// The config keys the client-credentials plugins read their references from.
const CLIENT_ID_REF: &str = "client_id_ref";
const CLIENT_SECRET_REF: &str = "client_secret_ref";
/// The config key of the client-credentials token endpoint.
const TOKEN_ENDPOINT: &str = "token_endpoint";
/// The config key of the space-separated scope list.
const SCOPES: &str = "scopes";
/// The config keys the RequiredHeaders guard reads (`inst-arh-01`).
const REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
const REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";
/// The header the RequestId transform propagates (`inst-ari-01`).
const REQUEST_ID: &str = "X-Request-ID";

/// The stages a guard or a transform is invoked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Before the upstream call.
    Request,
    /// On the upstream response.
    Response,
}

impl Stage {
    /// The wire token the request context records the phase with.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "on_request",
            Self::Response => "on_response",
        }
    }
}

/// A binding resolved to a runtime instance (`inst-alc-04`).
#[derive(Debug, Clone)]
struct ResolvedPlugin {
    /// The full GTS identifier the registry holds the plugin under.
    identifier: String,
    /// The registry key the behaviour dispatches on.
    name: String,
    /// The plugin type of the registry that resolved it.
    plugin_type: PluginType,
    /// The inline configuration pairs the binding carries.
    config: BTreeMap<String, String>,
}

impl ResolvedPlugin {
    /// The outcome record the request context carries for this plugin
    /// (`inst-alc-13`).
    fn outcome(&self, stage: Stage, outcome: &'static str) -> PluginOutcome {
        PluginOutcome {
            identifier: self.identifier.clone(),
            plugin_type: self.plugin_type.as_str(),
            phase: stage.as_str(),
            outcome,
        }
    }
}

/// The resolved chain, split into the stages its plugin types declare
/// (`inst-alc-02`).
#[derive(Debug, Default)]
struct ResolvedChain {
    auth: Vec<ResolvedPlugin>,
    guards: Vec<ResolvedPlugin>,
    transforms: Vec<ResolvedPlugin>,
}

/// The credential injection one auth plugin produced (`inst-ai-22`).
#[derive(Debug, Default)]
struct Injection {
    headers: Vec<(HeaderName, HeaderValue)>,
    query: Vec<(String, String)>,
    method_tag: &'static str,
}

/// The plugin chain executor
/// (`cpt-cf-oagw-dod-plugin-order:p1:inst-full`).
///
/// It holds the three registries entry 2.3 built, the credential source the
/// chain resolves its `cred://` references through, the token cache the
/// client-credentials plugins share and the process-local bucket table the rate
/// limit accounts. The cache and the buckets are the only state the request
/// path writes, and both are the process-local structures the feature owns.
pub struct ChainExecutor {
    auth: AuthPluginRegistry,
    guards: GuardPluginRegistry,
    transforms: TransformPluginRegistry,
    credentials: CredentialSource,
    tokens: Arc<TokenCache>,
    buckets: Arc<RateLimiter>,
    /// The request deadline every plugin invocation is bound by
    /// (`inst-alc-10`).
    budget: Duration,
    /// Whether the plaintext transport is admitted for the token endpoint.
    allow_insecure_transport: bool,
}

impl std::fmt::Debug for ChainExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credential source and the token cache are never rendered: the
        // chain is the one component that holds credential material.
        formatter
            .debug_struct("ChainExecutor")
            .field("budget_secs", &self.budget.as_secs())
            .field("buckets", &self.buckets.len())
            .finish_non_exhaustive()
    }
}

impl ChainExecutor {
    /// A chain over the builtin registries, `credentials` and the gear
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns the registry construction failure of a builtin identifier
    /// collision, which the mount step reports as a failed startup.
    pub fn new(
        config: &OagwConfig,
        credentials: CredentialSource,
    ) -> Result<Self, PluginCatalogError> {
        Ok(Self {
            auth: AuthPluginRegistry::with_builtins()?,
            guards: GuardPluginRegistry::with_builtins()?,
            transforms: TransformPluginRegistry::with_builtins()?,
            credentials,
            tokens: Arc::new(TokenCache::new(
                config.token_cache_capacity,
                Duration::from_secs(config.token_cache_ttl_secs),
            )),
            buckets: Arc::new(RateLimiter::new()),
            budget: Duration::from_secs(config.proxy_timeout_secs),
            allow_insecure_transport: config.allow_http_upstream,
        })
    }

    /// A chain over a credential source that resolves nothing.
    ///
    /// The mount installs it when the caller supplies no chain: every
    /// credential resolution fails closed with `500` SecretNotFound, so a mount
    /// without the credstore dependency never forwards an under-credentialed
    /// request (`cpt-cf-oagw-dod-cred-isolation:p1:inst-full`).
    ///
    /// # Errors
    ///
    /// Returns the registry construction failure of the builtins.
    pub fn standalone(config: &OagwConfig) -> Result<Self, PluginCatalogError> {
        Self::new(config, CredentialSource::unresolved())
    }

    /// The token cache, for the observability layer.
    #[must_use]
    pub const fn tokens(&self) -> &Arc<TokenCache> {
        &self.tokens
    }

    /// The bucket table, for the observability layer.
    #[must_use]
    pub const fn buckets(&self) -> &Arc<RateLimiter> {
        &self.buckets
    }

    /// Resolve the effective chain out of the merged configuration
    /// (`cpt-cf-oagw-algo-plugin-chain`).
    ///
    /// # Errors
    ///
    /// Returns `503` PluginNotFound when a binding or the effective `auth`
    /// declaration names an identifier no registry of its own type resolves: a
    /// catalog-only identifier, a UUID-backed custom definition with no runtime
    /// instance, or a name another type's registry holds. No plugin of the
    /// chain runs and no credential is resolved behind the failure
    /// (`inst-alc-06`, `inst-pc-05`).
    fn resolve_chain(
        &self,
        bindings: &[PluginBinding],
        effective: &EffectiveUpstream,
    ) -> Result<ResolvedChain, DomainError> {
        let mut chain = ResolvedChain::default();
        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-04
        // A binding that resolves to no runtime instance stops the resolution
        // and returns the `503` outcome: no plugin of the chain runs and no
        // credential is resolved behind the failure.
        for binding in bindings {
            let resolved = self.resolve_identifier(&binding.reference, binding_config(binding))?;
            match resolved.plugin_type {
                PluginType::Auth => chain.auth.push(resolved),
                PluginType::Guard => chain.guards.push(resolved),
                PluginType::Transform => chain.transforms.push(resolved),
            }
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-04

        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-06
        // The effective `auth` declaration is the auth plugin the upstream
        // configured: it resolves through the same registry and runs exactly
        // once, before every guard and transform of the request.
        if let Some(auth) = &effective.auth {
            let resolved = self.resolve_identifier(&auth.kind, auth.config.clone())?;
            if !chain.auth.iter().any(|plugin| plugin.name == resolved.name) {
                chain.auth.insert(0, resolved);
            }
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-06

        // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-14
        // The resolved chain is returned, or the `503` outcome the resolution
        // already turned into an error.
        Ok(chain)
        // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-14
    }

    /// Resolve one plugin identifier through the registry of its type
    /// (`inst-alc-03`, `inst-alc-04`).
    fn resolve_identifier(
        &self,
        raw: &str,
        config: BTreeMap<String, String>,
    ) -> Result<ResolvedPlugin, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-04
        // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-05
        // A named identifier resolves in the registry of its own type only, so
        // a name another type's registry holds is not found here. A
        // catalog-only identifier and a UUID-backed custom Starlark definition
        // have no runtime instance in this build, because no interpreter exists
        // (DECOMPOSITION assumption 4): both are the same `503` outcome.
        let parsed = PluginIdentifier::parse(raw).ok_or_else(|| not_found(raw))?;
        let (plugin_type, name) = match parsed.classify() {
            PluginClass::Named(plugin_type, name) => {
                let holds = match plugin_type {
                    PluginType::Auth => self.auth.contains(&name),
                    PluginType::Guard => self.guards.contains(&name),
                    PluginType::Transform => self.transforms.contains(&name),
                };
                holds.then_some((plugin_type, name))
            }
            PluginClass::CatalogOnly(..) | PluginClass::Custom(_) | PluginClass::Unknown => None,
        }
        .ok_or_else(|| not_found(raw))?;
        // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-05
        // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-04
        Ok(ResolvedPlugin {
            identifier: parsed.definition_id(plugin_type),
            name,
            plugin_type,
            config,
        })
    }
}

/// The `503` of an identifier no registry resolves (`inst-ai-02`).
fn not_found(reference: &str) -> DomainError {
    DomainError::PluginNotFound {
        detail: format!("the plugin `{reference}` resolves to no runtime instance"),
    }
}

/// The inline configuration pairs of a binding, as a sorted map of strings.
fn binding_config(binding: &PluginBinding) -> BTreeMap<String, String> {
    binding
        .config
        .as_ref()
        .and_then(|config| config.as_object())
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `Authorization` header member of a bearer token.
fn bearer(token: &SecretString) -> Option<(HeaderName, HeaderValue)> {
    Some((
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("{BEARER} {}", token.expose())).ok()?,
    ))
}

#[async_trait]
impl PluginChains for ChainExecutor {
    /// Execute the request-phase chain (`cpt-cf-oagw-flow-plugin-chain`).
    ///
    /// # Errors
    ///
    /// Returns the mapped error of the first stage that rejects, or the `504`
    /// of a plugin that outlived the request's remaining budget
    /// (`inst-alc-10`, `inst-alc-11`).
    async fn request_phase(
        &self,
        effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        request: &PluginRequest<'_>,
        context: &mut RequestContext,
    ) -> Result<RequestEffects, DomainError> {
        match tokio::time::timeout(
            self.budget,
            self.request_stage(effective, bindings, request, context),
        )
        .await
        {
            Ok(outcome) => outcome,
            // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-11
            // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-12
            // A plugin that outlives the request's remaining budget fails its
            // hook and carries no partial mutation forward: the effects are
            // discarded with the future that produced them, and the pipeline
            // classifies the response.
            Err(_) => Err(DomainError::RequestTimeout {
                detail: "the plugin chain outlived the request's remaining budget".to_owned(),
                retry_after_seconds: None,
            }),
            // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-12
            // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-11
        }
    }

    /// Execute the response-phase chain
    /// (`cpt-cf-oagw-algo-response-phase`).
    ///
    /// # Errors
    ///
    /// Returns the mapped error of the response-phase guards and transforms, or
    /// the `504` of a plugin that outlived the request's remaining budget
    /// (`inst-arp-10`, `inst-arp-11`).
    async fn response_phase(
        &self,
        effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        response: &ResponseContext,
        headers: &mut HeaderMap,
        context: &mut RequestContext,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-01
        // The pipeline classified the upstream response, or mapped the failure
        // it raised, and dispatches into this feature at the response-phase
        // hook with the response facts and the request context.
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-09
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-10
        // Every response-phase invocation runs inside the request's remaining
        // budget, like the request phase: a transform that outlives it or fails
        // discards its partial mutation.
        match tokio::time::timeout(
            self.budget,
            self.response_stage(effective, bindings, response, headers, context),
        )
        .await
        {
            Ok(outcome) => outcome,
            // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-11
            // The partial mutation dies with the future that produced it, and
            // the pipeline reports the mapped gateway error instead of a
            // partially transformed body.
            Err(_) => Err(DomainError::RequestTimeout {
                detail: "the response-phase chain outlived the request's remaining budget"
                    .to_owned(),
                retry_after_seconds: None,
            }),
            // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-11
        }
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-10
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-09
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-01
    }
}

impl ChainExecutor {
    /// The request stage, executed inside the plugin budget.
    async fn request_stage(
        &self,
        effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        request: &PluginRequest<'_>,
        context: &mut RequestContext,
    ) -> Result<RequestEffects, DomainError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-01
        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-02
        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-03
        // The hook point is reached after the configuration merge and before
        // endpoint selection, with the merged configuration, the ordered
        // binding list, the request facts and the request context.
        let chain = self.resolve_chain(bindings, effective)?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-03
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-02
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-01
        let upstream = context.upstream_id.clone().unwrap_or_default();
        let route = context.matched_route.clone().unwrap_or_default();
        let mut effects = RequestEffects::default();

        // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-09
        // Every stage runs in order, each plugin invocation receives the inline
        // configuration pairs its binding carried, and the first rejection stops
        // the chain: the `?` of the invocation is the return to the pipeline.
        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-05
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-01
        // The auth stage runs first: every credential the request forwards is
        // resolved and injected before a guard or a transform runs, and no
        // credential is resolved behind a stage that never runs.
        for plugin in &chain.auth {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-07
            // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-08
            // A rejection of the auth stage is returned to the pipeline as the
            // `401` of an IdP or credential refusal or as the `500` of a
            // reference that cannot be resolved, and no guard and no transform
            // of this request runs behind it.
            let injected = self.inject(plugin, request).await?;
            // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-08
            // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-07
            // @cpt-begin:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-13
            // The resolved auth method tag and the executed plugin identifier
            // are recorded on the request context for entry 2.7; neither is
            // credential material.
            context.auth_method = Some(injected.method_tag.to_owned());
            context.record_plugin(plugin.outcome(Stage::Request, "allow"));
            // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-13
            effects.headers.extend(injected.headers);
            effects.query.extend(injected.query);
        }
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-01
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-05

        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-09
        // The guard stage runs after the auth stage and before the transforms:
        // the RequiredHeaders decision reads the inbound header set only.
        for plugin in &chain.guards {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-10
            // A guard that rejects returns the rejection to the pipeline as a
            // `400` naming the first missing header, and no further guard and
            // no transform of this request runs behind it.
            let decision = self.guard(plugin, request.headers, Stage::Request, context)?;
            // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-13
            // Every executed guard is recorded with its outcome, in execution
            // order.
            context.record_plugin(plugin.outcome(Stage::Request, decision));
            // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-13
            // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-10
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-09

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-01
        // The guard stage reaches the rate-limit evaluation with the effective
        // rate-limit bound, the configured scope and strategy and the request
        // context.
        self.rate_limit(effective, &upstream, &route, request, context)
            .await?;
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-01

        // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-02
        // The CORS enforcement receives the effective `cors` configuration from
        // the merged configuration the hook was handed, after upstream
        // resolution and before forwarding.
        effects.cors = Some(self.cors(effective, request)?);
        // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-02

        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-12
        // The transform stage runs last on the request side, so a transform
        // observes the credentials the auth stage injected and the guard
        // decisions that admitted the request.
        for plugin in &chain.transforms {
            let mutations = self.transform(plugin, request.headers, Stage::Request, context)?;
            effects.headers.extend(mutations);
            // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-11
            // A transform that continued the chain is recorded like a guard,
            // with the outcome the invocation ended in.
            context.record_plugin(plugin.outcome(Stage::Request, "allow"));
            // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-11
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-12
        // @cpt-end:cpt-cf-oagw-algo-plugin-chain:p1:inst-alc-09

        // @cpt-begin:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-14
        // The mutated header set and query string are returned to the pipeline,
        // which continues with endpoint selection, the remaining validation and
        // the upstream call it owns.
        Ok(effects)
        // @cpt-end:cpt-cf-oagw-flow-plugin-chain:p1:inst-pc-14
    }

    /// The response stage (`cpt-cf-oagw-algo-response-phase`).
    async fn response_stage(
        &self,
        effective: &EffectiveUpstream,
        bindings: &[PluginBinding],
        response: &ResponseContext,
        headers: &mut HeaderMap,
        context: &mut RequestContext,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-02
        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-03
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-01
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-02
        // A response the pipeline handed to the entry-2.6 streaming path leaves
        // before this hook runs, so no response-phase plugin applies to the
        // exchange: the streamed session behaviour is entry 2.6's.
        if response.handed_off {
            return Ok(());
        }
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-02
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-01
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-03
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-02

        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-04
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-03
        // The upstream call succeeded and the response was classified: the
        // response half of the bound plugins runs against the upstream response
        // headers (`inst-rp-04`).
        let chain = self.resolve_chain(bindings, effective)?;

        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-05
        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-06
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-04
        // The RequiredHeaders decision of the response phase reads the upstream
        // response header set; a missing required header returns the `502` and
        // the upstream body is discarded with the response.
        for plugin in &chain.guards {
            let decision = self.guard(plugin, headers, Stage::Response, context)?;
            context.record_plugin(plugin.outcome(Stage::Response, decision));
        }
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-04
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-06
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-05

        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-08
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-05
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-12
        // The bound transforms that declare the response phase run in the same
        // upstream-before-route order the request phase used, and the response
        // header set is never a place a resolved secret reaches.
        for plugin in &chain.transforms {
            let mutations = self.transform(plugin, headers, Stage::Response, context)?;
            headers.extend(mutations);
            context.record_plugin(plugin.outcome(Stage::Response, "allow"));
        }
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-12
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-05
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-08

        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-06
        // The CORS headers of the request-phase decision are added by the
        // transport from the outcome it carries, `Vary: Origin` included.
        let _ = effective;
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-06

        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-11
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-13
        // The mutated response header set is returned to the pipeline for the
        // error-source stamping and the response.
        Ok(())
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-13
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-11
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-03
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-04
    }

    /// The guard decision of one bound guard plugin in one stage
    /// (`cpt-cf-oagw-algo-required-headers`).
    fn guard(
        &self,
        plugin: &ResolvedPlugin,
        headers: &HeaderMap,
        stage: Stage,
        context: &mut RequestContext,
    ) -> Result<&'static str, DomainError> {
        if plugin.name != REQUIRED_HEADERS_PLUGIN {
            // A guard this build has no behaviour for resolves in the registry
            // and declares itself, so it admits rather than failing a request
            // it has no decision for.
            return Ok("allow");
        }
        match stage {
            Stage::Request => required_request_headers(plugin, headers, context),
            Stage::Response => required_response_headers(plugin, headers, context),
        }
    }

    /// The mutation of one bound transform plugin in one stage
    /// (`cpt-cf-oagw-algo-request-id`).
    fn transform(
        &self,
        plugin: &ResolvedPlugin,
        headers: &HeaderMap,
        stage: Stage,
        context: &mut RequestContext,
    ) -> Result<Vec<(HeaderName, HeaderValue)>, DomainError> {
        if plugin.name != REQUEST_ID_PLUGIN {
            // A transform this build has no behaviour for resolves in the
            // registry and declares itself, so it mutates nothing rather than
            // failing a request it has no behaviour for.
            return Ok(Vec::new());
        }
        request_id(headers, stage, context)
    }

    /// The credential injection of one resolved auth plugin
    /// (`cpt-cf-oagw-algo-auth-inject`).
    async fn inject(
        &self,
        plugin: &ResolvedPlugin,
        request: &PluginRequest<'_>,
    ) -> Result<Injection, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-01
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-14
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-15
        // The dispatch is on the identifier the registry resolved. A rejection a
        // plugin raises is returned as the `401` AuthenticationFailed outcome,
        // or as the `500` SecretNotFound outcome when the cause was the
        // credential resolution, and neither detail carries credential material.
        let injection = match plugin.name.as_str() {
            // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-03
            // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-04
            // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-02
            // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-03
            // The no-op plugin injects nothing, resolves no credential and
            // succeeds, so the request proceeds with the credentials it already
            // carries.
            NOOP_PLUGIN => Ok(Injection {
                method_tag: TAG_NOOP,
                ..Injection::default()
            }),
            // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-03
            // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-02
            // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-04
            // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-03
            // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-04
            // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-05
            APIKEY_PLUGIN => self.api_key(plugin, request).await,
            // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-05
            // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-04
            // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-10
            // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-11
            OAUTH2_FORM_PLUGIN | OAUTH2_BASIC_PLUGIN => self.oauth2(plugin, request).await,
            // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-11
            // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-10
            _ => Err(not_found(&plugin.identifier)),
        }?;
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-15
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-14

        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-16
        // The injection is the only write this feature makes to the outbound
        // credential surface: no credential is written to the context, a log
        // line or a response.
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-16

        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-17
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-22
        // The mutated header set and query string are returned to the chain,
        // with the resolved secret material absent from the request context.
        Ok(injection)
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-22
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-17
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-01
    }

    /// The API key credential injection (`inst-ai-06` to `inst-ai-10`).
    async fn api_key(
        &self,
        plugin: &ResolvedPlugin,
        request: &PluginRequest<'_>,
    ) -> Result<Injection, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-05
        // The plugin configuration is the `cred://` reference for the key, the
        // injection target and the header name, which defaults to `X-API-Key`.
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-06
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-06
        // The configured `cred://` reference is resolved through the credential
        // source, scoped to the calling tenant.
        let secret = self
            .credentials
            .resolve(request.security, API_KEY_REF, value_of(plugin, API_KEY_REF))
            .await?;
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-06
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-06

        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-07
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-07
        if value_of(plugin, API_KEY_TARGET) == TARGET_QUERY {
            // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-08
            // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-08
            // Query injection: the secret is appended to the forwarded query
            // string and no header is added.
            return Ok(Injection {
                query: vec![(API_KEY_QUERY.to_owned(), secret.expose().to_owned())],
                method_tag: TAG_API_KEY,
                headers: Vec::new(),
            });
            // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-08
            // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-08
        }
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-07
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-07

        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-09
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-09
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-10
        // Header injection: the configured header, `X-API-Key` when the config
        // names none, carries the resolved secret.
        let name = api_key_header_name(plugin);
        let value = HeaderValue::from_str(secret.expose()).map_err(|_| DomainError::SecretNotFound {
            detail: format!("the credential reference of `{API_KEY_REF}` cannot be resolved"),
        })?;
        Ok(Injection {
            headers: vec![(name, value)],
            query: Vec::new(),
            method_tag: TAG_API_KEY,
        })
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-10
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-09
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-09
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-05
    }

    /// The client-credentials token injection (`inst-ai-12` to `inst-ai-21`).
    async fn oauth2(
        &self,
        plugin: &ResolvedPlugin,
        request: &PluginRequest<'_>,
    ) -> Result<Injection, DomainError> {
        let tag = if plugin.name == OAUTH2_BASIC_PLUGIN {
            TAG_BASIC
        } else {
            TAG_FORM
        };
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-12
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-12
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-13
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-14
        // The token is looked up in the cache, and a verified hit is served
        // with no credstore call and no IdP call.
        let token = self.obtain_token(plugin, request, tag).await?;
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-14
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-13
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-12
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-12

        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-20
        // The fetch returned a usable token: it is cached under the TTL rule the
        // token cache applies and injected on this request.
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-21
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-13
        // The token is injected as `Authorization: Bearer <token>` and nothing
        // of it reaches the request context, a log line or a response.
        Ok(Injection {
            headers: bearer(&token).into_iter().collect(),
            query: Vec::new(),
            method_tag: tag,
        })
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-13
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-21
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-20
    }

    /// Obtain the token through the cache (`cpt-cf-oagw-algo-token-cache`).
    async fn obtain_token(
        &self,
        plugin: &ResolvedPlugin,
        request: &PluginRequest<'_>,
        tag: &'static str,
    ) -> Result<Arc<SecretString>, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-01
        // The key is the subject tenant, the subject, the client-auth method
        // and a hash of the plugin configuration pairs, so upstreams that
        // differ in the endpoint or in the scopes get different entries.
        let hash = token_cache::config_hash(&plugin.config);
        let key = token_cache::cache_key(
            &request.tenant_id.to_string(),
            &request.security.subject_id().to_string(),
            tag,
            &hash,
        );
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-01

        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-02
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-03
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-05
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-06
        if let Some(token) = self.tokens.get(&key) {
            return Ok(token);
        }
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-06
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-05
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-03
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-02

        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-07
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-15
        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-16
        // A miss resolves both references inside the fetch path, and both
        // values are dropped when this scope ends.
        let client_id = self
            .credentials
            .resolve(request.security, CLIENT_ID_REF, value_of(plugin, CLIENT_ID_REF))
            .await?;
        let client_secret = self
            .credentials
            .resolve(
                request.security,
                CLIENT_SECRET_REF,
                value_of(plugin, CLIENT_SECRET_REF),
            )
            .await?;
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-16
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-15
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-07

        // @cpt-begin:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-17
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-08
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-09
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-10
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-11
        // The IdP is called once, with the configured client-auth method and the
        // configured space-separated scopes. A fetch failure, an IdP error status
        // and a response without a usable token are one `401`: nothing is cached
        // and the credentials are dropped with this scope, so the next request
        // for the same key retries the IdP (`inst-atc-11`).
        let fetched = fetch_token(self.token_config(plugin, &client_id, &client_secret))
            .await
            .map_err(|_| DomainError::AuthenticationFailed {
                detail: "the identity provider refused the client credentials".to_owned(),
            })?;
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-11
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-10
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-09
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-08
        // @cpt-end:cpt-cf-oagw-flow-auth-injection:p1:inst-ai-17

        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-12
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-13
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-14
        // The TTL is the smaller of the configured bound and the lifetime the
        // IdP reported minus the safety margin; a token whose lifetime leaves
        // nothing of it is injected for its own request and caches nothing.
        let token = Arc::new(fetched.bearer);
        let ttl = token_cache::entry_ttl(self.tokens.configured_ttl(), fetched.expires_in);
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-14
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-13
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-12

        if let Some(ttl) = ttl {
            // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-15
            // The token is stored as a `SecretString` under the computed TTL,
            // inside the `token_cache_capacity` bound.
            self.tokens.put(&key, Arc::clone(&token), Some(ttl));
            // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-15
        }
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-17
        // The token is handed to the injection only, never to the context.
        Ok(token)
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-17
    }

    /// The client-credentials configuration of one resolved plugin.
    ///
    /// The plugin configuration is the `token_endpoint` (or the `issuer_url`)
    /// and the two credential references, with the optional space-separated
    /// `scopes` (`inst-aai-11`).
    fn token_config(
        &self,
        plugin: &ResolvedPlugin,
        client_id: &SecretString,
        client_secret: &SecretString,
    ) -> OAuthClientConfig {
        // @cpt-begin:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-11
        // The plugin configuration is the `token_endpoint`, the two credential
        // references and the optional space-separated `scopes`.
        OAuthClientConfig {
            token_endpoint: Url::parse(value_of(plugin, TOKEN_ENDPOINT)).ok(),
            issuer_url: None,
            client_id: client_id.expose().to_owned(),
            client_secret: SecretString::new(client_secret.expose().to_owned()),
            scopes: value_of(plugin, SCOPES)
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            auth_method: if plugin.name == OAUTH2_BASIC_PLUGIN {
                ClientAuthMethod::Basic
            } else {
                ClientAuthMethod::Form
            },
            extra_headers: Vec::new(),
            refresh_offset: Duration::ZERO,
            jitter_max: Duration::ZERO,
            min_refresh_period: Duration::ZERO,
            default_ttl: Duration::from_secs(300),
            http_config: Some(self.token_http_config()),
        }
        // @cpt-end:cpt-cf-oagw-algo-auth-inject:p1:inst-aai-11
    }

    /// The HTTP configuration of the token request.
    fn token_http_config(&self) -> HttpClientConfig {
        let mut config = HttpClientConfig::token_endpoint();
        // The token request is issued once: the gateway maps a failure to the
        // `401` row and lets the next request retry the IdP, so no transport
        // retry hides a refusal from the accounting of the plugin
        // (`inst-ai-18`, `inst-ai-19`).
        config.retry = None;
        if self.allow_insecure_transport {
            // The plaintext posture the gear configuration opts into.
            config.transport = TransportSecurity::AllowInsecureHttp;
        }
        config
    }

    /// The rate-limit evaluation of the guard stage
    /// (`cpt-cf-oagw-algo-rate-limit`, `cpt-cf-oagw-algo-bucket-consume`).
    async fn rate_limit(
        &self,
        effective: &EffectiveUpstream,
        upstream: &str,
        route: &str,
        request: &PluginRequest<'_>,
        context: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let Some(config) = effective.rate_limit.as_ref() else {
            // No layer declares a rate limit: the request is unaccounted.
            return Ok(());
        };
        // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-10
        // The plan is the bucket key, the effective limit, the capacity and the
        // cost, with the scope the configuration selects.
        let (plan, scope, scope_fallback) = rate_limit::plan_and_scope(
            config,
            upstream,
            route,
            &request.tenant_id.to_string(),
            &request.security.subject_id().to_string(),
            request.peer_ip,
        );
        // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-10

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-02
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-03
        // The bucket table is consulted with the plan; a `queue` disposition is
        // held inside the queue-depth and budget bounds the limiter applies.
        let outcome = self.buckets.admit(&plan, self.budget).await;
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-03
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-02

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-04
        // The disposition the limiter returned selects the branch the flow
        // describes; every branch records its observation on the request
        // context, which is the surface the response phase reads.
        match outcome.decision {
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-05
            // The bucket satisfied the cost: the tokens are consumed, the
            // admission, the remaining tokens and the reset are recorded, and the
            // chain continues.
            RateDecision::Admitted => self.observe(&plan, scope, scope_fallback, &outcome, context),
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-05
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-12
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-13
            // The `degrade` strategy admits the request without consuming from
            // the exhausted bucket, and records the degradation for entry 2.7.
            RateDecision::Degraded => {
                self.observe(&plan, scope, scope_fallback, &outcome, context);
                context.degraded = true;
            }
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-13
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-12
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-14
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-15
            // The limiter computed the `Retry-After` from the time the bucket
            // needs to satisfy the cost, and the rate-limit header values from
            // the effective limit, the remaining tokens and the reset instant.
            RateDecision::Rejected => {
                self.observe(&plan, scope, scope_fallback, &outcome, context);
                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-16
                // The rejection stops the chain before the upstream call: no call
                // is made behind a `429`.
                return Err(DomainError::RateLimitExceeded {
                    detail: format!("the bucket `{}` cannot satisfy the request cost", plan.key),
                    retry_after_seconds: outcome.retry_after,
                });
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-16
            }
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-15
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-14
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-06
            // A queued disposition never leaves the limiter: the bounded wait
            // (`inst-rl-07`) resolves it into an admission when the tokens became
            // available inside both bounds (`inst-rl-08`, `inst-rl-09`), or into
            // the same rejection the `reject` strategy produces when a bound was
            // exceeded first (`inst-rl-10`, `inst-rl-11`).
            RateDecision::Queued => self.observe(&plan, scope, scope_fallback, &outcome, context),
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-06
        }
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-04
        Ok(())
    }

    /// Record the rate-limit observation of the request on the context, which is
    /// the outcome the response phase attaches its headers from
    /// (`inst-rl-17`, `inst-rp-10`).
    fn observe(
        &self,
        plan: &RatePlan,
        scope: RateScope,
        scope_fallback: bool,
        outcome: &RateOutcome,
        context: &mut RequestContext,
    ) {
        context.rate_limit = Some(RateLimitObservation {
            scope: scope.as_str().to_owned(),
            scope_fallback,
            decision: outcome.decision.as_str().to_owned(),
            limit: plan.limit,
            remaining: outcome.remaining,
            reset: outcome.reset,
            retry_after: outcome.retry_after,
            response_headers: plan.response_headers,
        });
    }

    /// The CORS enforcement of the request phase
    /// (`cpt-cf-oagw-algo-cors-enforce`).
    fn cors(
        &self,
        effective: &EffectiveUpstream,
        request: &PluginRequest<'_>,
    ) -> Result<CorsDecision, DomainError> {
        // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-03
        // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-01
        // A request without an `Origin`, or with one while the effective CORS
        // configuration is disabled, is not a CORS request for this feature.
        if !effective.cors_enabled() || request.origin.is_none() {
            // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-04
            // No CORS check runs, no CORS header is added and the chain
            // continues.
            return Ok(CorsDecision::same_origin());
            // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-04
        }
        // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-01
        // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-03

        // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-11
        // A preflight — an `OPTIONS` request with `Origin` and
        // `Access-Control-Request-Method` — was answered with the permissive
        // `204` of `cpt-cf-oagw-flow-proxy-preflight` before the pipeline
        // dispatched, and never reaches this decision.
        // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-11

        // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-05
        // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-06
        // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-04
        // @cpt-begin:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-12
        // The decision matches the origin exactly, admits `*` for any origin
        // and checks the method against `allowed_methods`.
        validate::validate_cors(effective, request.origin, request.method)
        // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-12
        // @cpt-end:cpt-cf-oagw-algo-cors-enforce:p1:inst-ace-04
        // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-06
        // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-05
    }
}

/// The RequiredHeaders decision of the request phase
/// (`cpt-cf-oagw-algo-required-headers`).
///
/// # Errors
///
/// Returns `400` ValidationError with REQUIRED_HEADER_MISSING in the problem
/// body when the first required request header is absent (`inst-arh-08`).
fn required_request_headers(
    plugin: &ResolvedPlugin,
    headers: &HeaderMap,
    context: &mut RequestContext,
) -> Result<&'static str, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-01
    // The two keys are independent: the request phase reads only
    // `required_request_headers`.
    let names = parsed_required(&plugin.config, REQUIRED_REQUEST_HEADERS);
    // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-01

    // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-02
    // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-03
    // An absent key, a blank value and an all-blank value are the fail-open
    // no-op of the phase.
    if names.is_empty() {
        return Ok("allow");
    }
    // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-03
    // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-02

    // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-05
    for name in &names {
        // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-06
        // Presence only: the value of the header is never read.
        // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-06
        // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-07
        if !headers.contains_key(name.as_str()) {
            // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-08
            // The rejection names the first missing header only and carries the
            // canonical error code of the guard in the problem body.
            context.error_code = Some(REQUIRED_HEADER_MISSING);
            return Err(DomainError::ValidationError {
                detail: format!("the required request header `{name}` is absent"),
            });
            // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-08
        }
        // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-07
    }
    // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-05

    // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-09
    Ok("allow")
    // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-09
}

/// The RequiredHeaders decision of the response phase
/// (`cpt-cf-oagw-algo-required-headers`).
///
/// # Errors
///
/// Returns `502` DownstreamError with REQUIRED_HEADER_MISSING in the problem
/// body when the first required response header is absent (`inst-rp-07`).
fn required_response_headers(
    plugin: &ResolvedPlugin,
    headers: &HeaderMap,
    context: &mut RequestContext,
) -> Result<&'static str, DomainError> {
    let names = parsed_required(&plugin.config, REQUIRED_RESPONSE_HEADERS);
    if names.is_empty() {
        return Ok("allow");
    }
    for name in &names {
        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-07
        if !headers.contains_key(name.as_str()) {
            // The upstream body is discarded with the response: the mapped
            // error is the outcome the pipeline reports.
            context.error_code = Some(REQUIRED_HEADER_MISSING);
            return Err(DomainError::DownstreamError {
                detail: format!("the required response header `{name}` is absent"),
            });
        }
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-07
    }
    Ok("allow")
}

/// The configured header names of one key, parsed per `inst-arh-04`.
fn parsed_required(config: &BTreeMap<String, String>, key: &str) -> Vec<String> {
    // @cpt-begin:cpt-cf-oagw-algo-required-headers:p1:inst-arh-04
    // The value is parsed in order: split on `,`, trim each entry, lowercase
    // it and drop the empty entries, so a blank member never becomes a
    // required header name.
    config
        .get(key)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
    // @cpt-end:cpt-cf-oagw-algo-required-headers:p1:inst-arh-04
}

/// The RequestId mutation of one stage (`cpt-cf-oagw-algo-request-id`).
fn request_id(
    headers: &HeaderMap,
    stage: Stage,
    context: &mut RequestContext,
) -> Result<Vec<(HeaderName, HeaderValue)>, DomainError> {
    match stage {
        Stage::Request => {
            // @cpt-begin:cpt-cf-oagw-algo-request-id:p1:inst-ari-01
            // @cpt-begin:cpt-cf-oagw-algo-request-id:p1:inst-ari-02
            let carried = headers
                .get(REQUEST_ID)
                .map(|value| value.to_str().unwrap_or_default().to_owned())
                .filter(|value| !value.is_empty());
            // @cpt-end:cpt-cf-oagw-algo-request-id:p1:inst-ari-02
            // @cpt-end:cpt-cf-oagw-algo-request-id:p1:inst-ari-01

            // @cpt-begin:cpt-cf-oagw-algo-request-id:p1:inst-ari-03
            // @cpt-begin:cpt-cf-oagw-algo-request-id:p1:inst-ari-04
            // A carried value is propagated unchanged; an absent one is
            // replaced by a fresh identifier.
            let identifier = carried.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            // @cpt-end:cpt-cf-oagw-algo-request-id:p1:inst-ari-04
            // @cpt-end:cpt-cf-oagw-algo-request-id:p1:inst-ari-03

            // @cpt-begin:cpt-cf-oagw-algo-request-id:p1:inst-ari-05
            // The identifier is recorded on the request context, which is the
            // correlation identifier entry 2.7 logs.
            context.request_id = Some(identifier.clone());
            // @cpt-end:cpt-cf-oagw-algo-request-id:p1:inst-ari-05
            Ok(request_id_header(&identifier))
        }
        Stage::Response => {
            // @cpt-begin:cpt-cf-oagw-algo-request-id:p1:inst-ari-06
            // The response carries the identifier the request was served with,
            // so a caller can correlate a response with its request.
            let identifier = context
                .request_id
                .clone()
                .unwrap_or_else(|| context.trace_id.clone());
            // @cpt-end:cpt-cf-oagw-algo-request-id:p1:inst-ari-06
            Ok(request_id_header(&identifier))
        }
        // `inst-ari-07`: no other mutation is applied, and `inst-ari-08` is the
        // return of the mutated header sets.
    }
}

/// The `X-Request-ID` header member of an identifier.
fn request_id_header(identifier: &str) -> Vec<(HeaderName, HeaderValue)> {
    let name = HeaderName::from_bytes(REQUEST_ID.as_bytes());
    let value = HeaderValue::from_str(identifier);
    match (name, value) {
        (Ok(name), Ok(value)) => vec![(name, value)],
        _ => Vec::new(),
    }
}

/// The string value of a configuration key, or the empty string.
fn value_of<'a>(plugin: &'a ResolvedPlugin, key: &str) -> &'a str {
    plugin.config.get(key).map(String::as_str).unwrap_or("")
}

/// The name of the API key header the config names, or the recorded default.
fn api_key_header_name(plugin: &ResolvedPlugin) -> HeaderName {
    let name = value_of(plugin, API_KEY_HEADER);
    HeaderName::from_bytes(name.as_bytes())
        .unwrap_or_else(|_| HeaderName::from_static(DEFAULT_API_KEY_HEADER))
}

// @cpt-begin:cpt-cf-oagw-dod-plugin-order:p2:inst-full
#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::domain::model::{AuthConfig, HeadersConfig, RateLimitConfig, Sharing};
    use crate::infra::proxy::context::RequestContext;
    use crate::infra::proxy::credentials::CredentialSource;

    /// The identifiers the tests bind.
    const GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    const TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
    const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    const CATALOG_ONLY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";

    /// The tenant every test request runs for.
    const TEST_TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000070");

    /// The executor over an empty credential store.
    fn executor() -> ChainExecutor {
        ChainExecutor::new(
            &OagwConfig::default(),
            CredentialSource::new(Arc::new(MockCredStoreClient::with_secrets(vec![(
                "partner-api-key".to_owned(),
                "sk-stub-3f91a".to_owned(),
            )]))),
        )
        .expect("the chain builds")
    }

    /// A security context for `tenant`.
    fn security(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_type("user")
            .subject_tenant_id(tenant)
            .build()
            .expect("context builds")
    }

    /// The facts of a `GET` request with `headers`.
    fn request<'a>(
        security: &'a SecurityContext,
        headers: &'a HeaderMap,
    ) -> PluginRequest<'a> {
        PluginRequest {
            security,
            tenant_id: security.subject_tenant_id(),
            method: "GET",
            query: None,
            headers,
            origin: None,
            peer_ip: None,
        }
    }

    /// The effective upstream the chain runs for, with `auth` and `bindings`.
    fn effective(auth: Option<AuthConfig>, bindings: Vec<PluginBinding>) -> EffectiveUpstream {
        EffectiveUpstream {
            headers: HeadersConfig::default(),
            cors: None,
            rate_limit: None,
            plugins: bindings,
            auth,
            enforced: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// A binding of `reference` with an inline config.
    fn binding(reference: &str, config: &[(&str, &str)]) -> PluginBinding {
        PluginBinding {
            position: 0,
            reference: reference.to_owned(),
            plugin_uuid: None,
            config: Some(serde_json::Value::Object(
                config
                    .iter()
                    .map(|(key, value)| (key.to_string(), serde_json::Value::from(*value)))
                    .collect(),
            )),
        }
    }

    /// The auth declaration of `kind`.
    fn auth(kind: &str, config: &[(&str, &str)]) -> AuthConfig {
        AuthConfig {
            kind: kind.to_owned(),
            sharing: Sharing::Private,
            config: config
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    async fn run(executor: &ChainExecutor, effective: &EffectiveUpstream) -> (Result<RequestEffects, DomainError>, RequestContext) {
        // One tenant identity for the whole suite: the bucket key of two runs
        // over the same plan is the same one.
        let security = security(TEST_TENANT);
        let headers = HeaderMap::new();
        let mut context = RequestContext::new(
            "trace".to_owned(),
            "/v1".to_owned(),
            "GET".to_owned(),
        );
        context.upstream_id = Some("00000000-0000-0000-0000-000000000001".to_owned());
        let outcome = executor
            .request_phase(
                effective,
                &effective.plugins,
                &request(&security, &headers),
                &mut context,
            )
            .await;
        (outcome, context)
    }

    /// A binding naming a catalog-only auth plugin has no runtime instance: the
    /// chain refuses it with `503` before any stage runs.
    #[tokio::test]
    async fn a_catalog_only_auth_identifier_is_a_plugin_not_found() {
        let executor = executor();
        let effective = effective(
            Some(auth(CATALOG_ONLY, &[("secret_ref", "cred://partner-api-key")])),
            Vec::new(),
        );

        let (outcome, context) = run(&executor, &effective).await;
        let error = outcome.expect_err("the identifier resolves to nothing");
        assert_eq!(error.status(), 503, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
        assert!(context.plugins.is_empty(), "no stage ran");
    }

    /// A UUID-backed custom Starlark definition has no runtime instance either,
    /// because no interpreter exists in this build: the same `503`.
    #[tokio::test]
    async fn a_custom_plugin_identifier_is_a_plugin_not_found() {
        let executor = executor();
        let custom = Uuid::new_v4().to_string();
        let effective = effective(
            Some(auth(APIKEY, &[("secret_ref", "cred://partner-api-key")])),
            vec![binding(&custom, &[])],
        );

        let (outcome, context) = run(&executor, &effective).await;
        let error = outcome.expect_err("the identifier resolves to nothing");
        assert_eq!(error.status(), 503, "{error}");
        assert!(context.plugins.is_empty(), "no stage ran");
    }

    /// An identifier another type's registry holds is not found by the type the
    /// binding names.
    #[tokio::test]
    async fn a_guard_identifier_is_not_found_in_the_transform_registry() {
        let executor = executor();
        let effective = effective(None, vec![binding(GUARD, &[])]);

        let (outcome, _) = run(&executor, &effective).await;
        assert!(outcome.is_ok(), "the guard resolves in its own registry");
    }

    /// The chain runs the auth stage, then the guards, then the transforms, in
    /// binding order, and records every executed plugin.
    #[tokio::test]
    async fn the_stages_run_in_order_and_are_recorded() {
        let executor = executor();
        let effective = effective(
            Some(auth(APIKEY, &[("secret_ref", "cred://partner-api-key")])),
            vec![binding(GUARD, &[]), binding(TRANSFORM, &[])],
        );

        let (outcome, context) = run(&executor, &effective).await;
        let effects = outcome.expect("every stage allowed the request");
        let identifiers: Vec<&str> = context
            .plugins
            .iter()
            .map(|outcome| outcome.identifier.as_str())
            .collect();
        assert_eq!(
            identifiers,
            vec![APIKEY, GUARD, TRANSFORM],
            "auth, then guards, then transforms"
        );
        assert_eq!(context.auth_method.as_deref(), Some("api_key"));
        assert_eq!(effects.headers.len(), 2, "the credential and the identifier");
        assert_eq!(effects.headers[0].0.as_str(), "x-api-key");
        assert_eq!(effects.headers[1].0.as_str(), "x-request-id");
        assert!(
            context
                .plugins
                .iter()
                .all(|outcome| outcome.outcome == "allow"),
            "every stage continued the chain"
        );
    }

    /// A guard that rejects stops the chain at the first rejection: the stages
    /// behind it never run and nothing is recorded for them.
    #[tokio::test]
    async fn the_chain_stops_at_the_first_rejection() {
        let executor = executor();
        let effective = effective(
            Some(auth(APIKEY, &[("secret_ref", "cred://partner-api-key")])),
            vec![
                binding(GUARD, &[("required_request_headers", "x-proof")]),
                binding(TRANSFORM, &[]),
            ],
        );

        let (outcome, context) = run(&executor, &effective).await;
        let error = outcome.expect_err("the guard rejected the request");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            context.error_code,
            Some(crate::infra::proxy::context::REQUIRED_HEADER_MISSING)
        );
        assert!(
            context
                .plugins
                .iter()
                .all(|outcome| outcome.plugin_type != "transform"),
            "the transform stage never ran"
        );
        assert!(!context.degraded, "no rate limit ran");
    }

    /// The rate limit of the effective configuration is consumed by the guard
    /// stage, and a rejected request is recorded as throttled.
    #[tokio::test]
    async fn an_exhausted_bucket_is_a_429_and_is_recorded() {
        let executor = executor();
        let mut effective = effective(None, Vec::new());
        effective.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: crate::domain::model::SustainedRate {
                rate: 1,
                window: crate::domain::model::RateWindow::Second,
            },
            burst: Some(crate::domain::model::BurstRate { capacity: Some(1) }),
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        });

        let (first, first_context) = run(&executor, &effective).await;
        assert!(first.is_ok(), "the first request is admitted");
        assert!(!first_context.degraded, "an admitted request is not degraded");
        let (second, second_context) = run(&executor, &effective).await;
        let error = second.expect_err("the bucket is exhausted");
        assert_eq!(error.status(), 429, "{error}");
        let observation = second_context.rate_limit.expect("the request was observed");
        assert_eq!(observation.decision, "rejected");
        assert_eq!(observation.scope, "tenant");
        assert!(!observation.scope_fallback, "the tenant scope was available");
    }

    /// A peer address the host did not provide makes the `ip` scope fall back
    /// to the tenant key, and the request records the fallback.
    #[tokio::test]
    async fn an_ip_scope_without_a_peer_falls_back_to_the_tenant_key() {
        let executor = executor();
        let mut effective = effective(None, Vec::new());
        effective.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: crate::domain::model::SustainedRate {
                rate: 1,
                window: crate::domain::model::RateWindow::Second,
            },
            burst: Some(crate::domain::model::BurstRate { capacity: Some(1) }),
            scope: crate::domain::model::RateScope::Ip,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        });

        let (_, context) = run(&executor, &effective).await;
        let observation = context.rate_limit.expect("the request was observed");
        assert_eq!(observation.scope, "tenant");
        assert!(observation.scope_fallback, "the peer address was unavailable");
    }

    /// A CORS request under a disabled configuration is not a CORS request: no
    /// check runs and the decision adds no CORS header.
    #[tokio::test]
    async fn an_origin_under_a_disabled_configuration_is_not_a_cors_request() {
        let executor = executor();
        let security = security(Uuid::new_v4());
        let headers = HeaderMap::new();
        let mut context = RequestContext::new(
            "trace".to_owned(),
            "/v1".to_owned(),
            "GET".to_owned(),
        );
        let effective = effective(None, Vec::new());
        let request = PluginRequest {
            security: &security,
            tenant_id: security.subject_tenant_id(),
            method: "GET",
            query: None,
            headers: &headers,
            origin: Some("https://app.example.com"),
            peer_ip: None,
        };

        let effects = executor
            .request_phase(&effective, &[], &request, &mut context)
            .await
            .expect("no CORS check runs");
        let decision = effects.cors.expect("the chain decided");
        assert!(decision.allow_origin.is_empty(), "no CORS header is added");
    }
}
// @cpt-end:cpt-cf-oagw-dod-plugin-order:p2:inst-full
