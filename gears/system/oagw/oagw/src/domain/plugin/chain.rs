//! Deterministic plugin chain execution (algorithm
//! `cpt-cf-oagw-algo-plugin-system-execute-chain`,
//! DoD `cpt-cf-oagw-dod-plugin-system-execution-order`).
//!
//! A [`PluginChain`] is built from the effective (already merged,
//! ancestor-first) binding list.  Execution never re-orders bindings at
//! runtime: bindings are partitioned by their GTS plugin type and run in the
//! fixed order
//!
//! ```text
//! Auth → Guards → Transform(request) → upstream → Transform(response/error)
//! ```
//!
//! (steps `inst-ps-chain-order` .. `inst-ps-chain-return`).  Because auth is
//! single-value credential injection ("executed once per request", ADR 0002),
//! a chain that resolves more than one auth binding is rejected as a binding
//! **conflict** at build time (DoD
//! `cpt-cf-oagw-dod-plugin-system-identification`).

use std::sync::Arc;

use crate::domain::entity::config::PluginBinding;
use crate::domain::entity::plugin::PluginType;
use crate::domain::error::DomainError;
use crate::domain::plugin::registries::{CustomPluginLookup, PluginRegistries, ResolvedPlugin};
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPhase, GuardPlugin, GuardRejection,
    RequestContext, ResponseContext, TransformPlugin,
};
use uuid::Uuid;

/// A typed chain-construction/execution failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginChainError {
    /// A binding could not be resolved (catalog-only, missing, or malformed).
    #[error("plugin not found: {detail}")]
    PluginNotFound { detail: String },
    /// More than one auth plugin is bound — auth is single-value credential
    /// injection (conflict detection).
    #[error("auth plugin conflict: {plugin_refs}")]
    AuthConflict { plugin_refs: String },
    /// The binding list violates the contiguous-from-0 position rule.
    #[error("binding validation failed: {detail}")]
    InvalidBinding { detail: String },
    /// A UUID-backed custom plugin needs the Starlark sandbox to run.
    #[error("custom plugin '{name}' execution requires the Starlark sandbox")]
    CustomExecutionUnavailable { plugin_ref: String, name: String },
}

impl PluginChainError {
    /// Maps a chain failure to the DESIGN error catalog.
    #[must_use]
    pub fn to_domain_error(&self) -> DomainError {
        match self {
            Self::PluginNotFound { detail }
            | Self::CustomExecutionUnavailable {
                plugin_ref: detail, ..
            } => DomainError::PluginNotFound {
                detail: detail.clone(),
            },
            Self::AuthConflict { plugin_refs }
            | Self::InvalidBinding {
                detail: plugin_refs,
            } => DomainError::validation(None, plugin_refs.clone()),
        }
    }
}

/// The resolved, ordered plugin chain.
///
/// Bindings are partitioned by type but the within-type order is preserved
/// from the effective binding list (upstream plugins precede route plugins —
/// the merge layer already concatenated ancestor-first).
#[derive(Debug)]
pub struct PluginChain {
    /// Auth bindings — at most one (constructor rejects conflicts).
    auth: Vec<(PluginBinding, Arc<dyn AuthPlugin>)>,
    /// Guard bindings.
    guards: Vec<(PluginBinding, Arc<dyn GuardPlugin>)>,
    /// Transform bindings.
    transforms: Vec<(PluginBinding, Arc<dyn TransformPlugin>)>,
    /// UUID-backed custom bindings (identification validated; execution is
    /// delivered with the Starlark sandbox, p3).
    customs: Vec<(PluginBinding, PluginType, Uuid, String)>,
}

/// Result of running the request half of the chain (auth → guards →
/// transform(request)).
#[derive(Debug)]
pub enum ChainOutcome {
    /// The request may proceed to the upstream call.
    Proceed,
    /// A guard rejected the request before the upstream call.
    Rejected(GuardRejection),
    /// The chain failed with a gateway error (auth or transform).
    Failed(DomainError),
}

/// Result of running the response half of the chain.
#[derive(Debug)]
pub enum ResponseOutcome {
    Approved,
    GuardRejected(GuardRejection),
    Failed(DomainError),
}

impl PluginChain {
    /// Builds a chain: resolves every binding, partitions by type, and
    /// enforces the auth-singleton conflict rule and the
    /// contiguous-from-0 position rule.
    ///
    /// # Errors
    /// - [`PluginChainError::InvalidBinding`] — positions not contiguous from
    ///   0 (DoD `cpt-cf-oagw-dod-plugin-system-identification`);
    /// - [`PluginChainError::AuthConflict`] — more than one auth plugin;
    /// - [`PluginChainError::PluginNotFound`] — an unresolvable binding.
    pub async fn build(
        bindings: &[PluginBinding],
        registries: &PluginRegistries,
        tenant_id: Uuid,
        custom: Option<&dyn CustomPluginLookup>,
    ) -> Result<Self, PluginChainError> {
        Self::validate_positions(bindings)?;

        let mut auth: Vec<(PluginBinding, Arc<dyn AuthPlugin>)> = Vec::new();
        let mut guards: Vec<(PluginBinding, Arc<dyn GuardPlugin>)> = Vec::new();
        let mut transforms: Vec<(PluginBinding, Arc<dyn TransformPlugin>)> = Vec::new();
        let mut customs: Vec<(PluginBinding, PluginType, Uuid, String)> = Vec::new();

        for binding in bindings {
            let resolved = registries
                .resolve_binding(tenant_id, &binding.plugin_ref, binding.plugin_uuid, custom)
                .await
                .map_err(|e| PluginChainError::PluginNotFound {
                    detail: e.to_string(),
                })?;
            match resolved {
                ResolvedPlugin::Auth(p) => auth.push((binding.clone(), p)),
                ResolvedPlugin::Guard(p) => guards.push((binding.clone(), p)),
                ResolvedPlugin::Transform(p) => transforms.push((binding.clone(), p)),
                ResolvedPlugin::Custom { plugin_id, name } => {
                    let parsed =
                        crate::domain::plugin::ids::GtsPluginRef::parse(&binding.plugin_ref)
                            .ok_or_else(|| PluginChainError::InvalidBinding {
                                detail: format!("invalid ref '{}'", binding.plugin_ref),
                            })?;
                    customs.push((binding.clone(), parsed.plugin_type, plugin_id, name));
                }
            }
        }

        // Auth is single-value credential injection: more than one auth
        // binding is a conflict (conflict detection).
        let auth_refs = auth
            .iter()
            .map(|(b, _)| b.plugin_ref.as_str())
            .collect::<Vec<_>>();
        let custom_auth = customs
            .iter()
            .filter(|(_, t, _, _)| *t == PluginType::Auth)
            .map(|(b, _, _, _)| b.plugin_ref.as_str())
            .collect::<Vec<_>>();
        if auth.len() + custom_auth.len() > 1 {
            let mut refs = auth_refs;
            refs.extend(custom_auth);
            return Err(PluginChainError::AuthConflict {
                plugin_refs: refs.join(", "),
            });
        }

        Ok(Self {
            auth,
            guards,
            transforms,
            customs,
        })
    }

    fn validate_positions(bindings: &[PluginBinding]) -> Result<(), PluginChainError> {
        for (expected, b) in (0_u32..).zip(bindings) {
            if b.position != expected {
                return Err(PluginChainError::InvalidBinding {
                    detail: format!(
                        "positions must be contiguous from 0: expected {expected}, got {}",
                        b.position
                    ),
                });
            }
        }
        Ok(())
    }

    /// Number of resolved built-in plugins in the chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.auth.len() + self.guards.len() + self.transforms.len() + self.customs.len()
    }

    /// Whether the chain has no bindings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Auth phase — injects credentials once per request (step
    /// `inst-ps-chain-auth`).
    ///
    /// Each binding's own `config` blob is threaded into the context for the
    /// invocation (the effective binding list carries per-binding
    /// configuration; the Data Plane hands a pristine context and the chain
    /// isolates the last binding's config from the next).
    ///
    /// # Errors
    /// Auth failures surface the plugin's [`DomainError`]
    /// (`authentication.failed` / `secret.not_found`); a custom auth plugin
    /// needs the Starlark sandbox (503 until p3).
    pub async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        for (binding, plugin) in &self.auth {
            ctx.config = binding.config.clone();
            plugin.authenticate(ctx).await?;
        }
        // Custom auth plugins execute in the sandbox (p3); a custom auth
        // binding is a single-binding chain by construction.
        if let Some((_, _, _, name)) = self
            .customs
            .iter()
            .find(|(_, t, _, _)| *t == PluginType::Auth)
        {
            return Err(PluginChainError::CustomExecutionUnavailable {
                plugin_ref: "custom-auth".to_owned(),
                name: name.clone(),
            }
            .to_domain_error());
        }
        Ok(())
    }

    /// Guard request phase — first rejection stops the chain (step
    /// `inst-ps-chain-guard-req`).
    ///
    /// Each binding's `config` is applied by handing the guard a cloned
    /// context carrying that binding's configuration (the request surface
    /// itself is read-only here).
    pub async fn guard_request(&self, ctx: &RequestContext) -> Option<GuardRejection> {
        for (binding, plugin) in &self.guards {
            let mut bound = ctx.clone();
            bound.config = binding.config.clone();
            match plugin.guard_request(&bound).await {
                GuardDecision::Allow => {}
                GuardDecision::Reject(rejection) => return Some(rejection),
            }
        }
        None
    }

    /// Transform request phase (step `inst-ps-chain-transform-req`).
    ///
    /// Each binding's `config` is threaded into the context for its own
    /// invocation.
    ///
    /// # Errors
    /// A transform failure aborts the request with a gateway error.
    pub async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        for (binding, plugin) in &self.transforms {
            ctx.config = binding.config.clone();
            plugin.transform_request(ctx).await?;
        }
        self.run_custom_transforms().await
    }

    /// Guard response phase (step `inst-ps-chain-guard-resp`).
    ///
    /// Each binding's `config` is applied by handing the guard a cloned
    /// context carrying that binding's configuration.
    pub async fn guard_response(&self, ctx: &ResponseContext) -> Option<GuardRejection> {
        for (binding, plugin) in &self.guards {
            let mut bound = ctx.clone();
            bound.config = binding.config.clone();
            match plugin.guard_response(&bound).await {
                GuardDecision::Allow => {}
                GuardDecision::Reject(rejection) => {
                    let mut r = rejection;
                    r.phase = GuardPhase::Response;
                    return Some(r);
                }
            }
        }
        None
    }

    /// Transform response phase (step `inst-ps-chain-transform-resp`).
    ///
    /// Each binding's `config` is threaded into the context for its own
    /// invocation.
    ///
    /// # Errors
    /// A transform failure produces a gateway error response.
    pub async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError> {
        for (binding, plugin) in &self.transforms {
            ctx.config = binding.config.clone();
            plugin.transform_response(ctx).await?;
        }
        self.run_custom_transforms().await
    }

    /// Transform error phase — invoked when the upstream call fails (step
    /// `inst-ps-chain-transform-resp`).
    ///
    /// Each binding's `config` is threaded into the context for its own
    /// invocation.
    ///
    /// # Errors
    pub async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), DomainError> {
        for (binding, plugin) in &self.transforms {
            ctx.config = binding.config.clone();
            plugin.transform_error(ctx).await?;
        }
        self.run_custom_transforms().await
    }

    /// Runs the full request half: auth → guards → transform(request).
    ///
    /// `custom` supply chain as needed; returns the first guard rejection or
    /// the first auth/transform failure.
    pub async fn run_request_chain(&self, ctx: &mut RequestContext) -> ChainOutcome {
        if let Err(err) = self.authenticate(ctx).await {
            return ChainOutcome::Failed(err);
        }
        if let Some(rejection) = self.guard_request(ctx).await {
            return ChainOutcome::Rejected(rejection);
        }
        if let Err(err) = self.transform_request(ctx).await {
            return ChainOutcome::Failed(err);
        }
        ChainOutcome::Proceed
    }

    /// Runs the response half: guards then transform(response).
    pub async fn run_response_chain(&self, ctx: &mut ResponseContext) -> ResponseOutcome {
        if let Some(rejection) = self.guard_response(ctx).await {
            return ResponseOutcome::GuardRejected(rejection);
        }
        if let Err(err) = self.transform_response(ctx).await {
            return ResponseOutcome::Failed(err);
        }
        ResponseOutcome::Approved
    }

    /// Custom transform execution is delivered with the Starlark sandbox
    /// (p3): a custom guard/transform binding returns a 503 until then,
    /// while built-in chains are unaffected.
    async fn run_custom_transforms(&self) -> Result<(), DomainError> {
        if let Some((_, _, _, name)) = self
            .customs
            .iter()
            .find(|(_, t, _, _)| *t != PluginType::Auth)
        {
            return Err(PluginChainError::CustomExecutionUnavailable {
                plugin_ref: "custom-transform".to_owned(),
                name: name.clone(),
            }
            .to_domain_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::Headers;
    use crate::domain::plugin::ids::{
        APIKEY_AUTH, NOOP_AUTH, REQUEST_ID_TRANSFORM, REQUIRED_HEADERS_GUARD,
    };
    use crate::infra::plugin::builtin_registries;
    use serde_json::json;

    /// Built-in registries backed by an empty CredStore so every built-in
    /// (apikey, oauth2) registers.
    fn regs() -> PluginRegistries {
        builtin_registries(Some(Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::empty(),
        )))
    }

    fn binding(position: u32, plugin_ref: &str, uuid: Option<Uuid>) -> PluginBinding {
        PluginBinding {
            position,
            plugin_ref: plugin_ref.to_owned(),
            plugin_uuid: uuid,
            config: json!({}),
        }
    }

    #[tokio::test]
    async fn empty_chain_builds_and_is_empty() {
        let regs = regs();
        let chain = PluginChain::build(&[], &regs, Uuid::nil(), None)
            .await
            .expect("empty chain builds");
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
    }

    #[tokio::test]
    async fn full_chain_builds_from_builtins() {
        let regs = regs();
        let bindings = [
            binding(0, APIKEY_AUTH, None),
            binding(1, REQUIRED_HEADERS_GUARD, None),
            binding(2, REQUEST_ID_TRANSFORM, None),
        ];
        let chain = PluginChain::build(&bindings, &regs, Uuid::nil(), None)
            .await
            .expect("builtins resolve");
        assert_eq!(chain.len(), 3);
    }

    #[tokio::test]
    async fn catalog_only_binding_fails_resolution() {
        let regs = regs();
        let bindings = [binding(
            0,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
            None,
        )];
        let err = PluginChain::build(&bindings, &regs, Uuid::nil(), None)
            .await
            .expect_err("catalog-only must not resolve");
        assert!(matches!(err, PluginChainError::PluginNotFound { .. }));
        assert_eq!(err.to_domain_error().status(), 503);
    }

    #[tokio::test]
    async fn two_auth_bindings_are_a_conflict() {
        let regs = regs();
        let bindings = [binding(0, NOOP_AUTH, None), binding(1, APIKEY_AUTH, None)];
        let err = PluginChain::build(&bindings, &regs, Uuid::nil(), None)
            .await
            .expect_err("more than one auth is a conflict");
        assert!(
            matches!(err, PluginChainError::AuthConflict { .. }),
            "{err:?}"
        );
        // Maps to a binding-validation 400.
        assert_eq!(err.to_domain_error().status(), 400);
    }

    #[tokio::test]
    async fn non_contiguous_positions_are_rejected() {
        let regs = regs();
        let bindings = [
            binding(0, REQUIRED_HEADERS_GUARD, None),
            binding(2, REQUEST_ID_TRANSFORM, None),
        ];
        let err = PluginChain::build(&bindings, &regs, Uuid::nil(), None)
            .await
            .expect_err("positions must be contiguous");
        assert!(matches!(err, PluginChainError::InvalidBinding { .. }));
    }

    #[tokio::test]
    async fn request_chain_runs_auth_guard_transform_in_order() {
        let regs = regs();
        let bindings = [
            binding(0, APIKEY_AUTH, None),
            binding(1, REQUIRED_HEADERS_GUARD, None),
            binding(2, REQUEST_ID_TRANSFORM, None),
        ];
        let chain = PluginChain::build(&bindings, &regs, Uuid::nil(), None)
            .await
            .expect("builds");

        let mut ctx = RequestContext {
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            config: json!({}),
            ..RequestContext::default()
        };
        // apikey without a resolved secret in config → guard/transform order
        // still exercised with an empty config (apikey is a no-op on missing
        // config); required_headers with no configured list is fail-open.
        let outcome = chain.run_request_chain(&mut ctx).await;
        assert!(matches!(outcome, ChainOutcome::Proceed));
    }

    #[tokio::test]
    async fn ac_27_guard_rejection_stops_the_chain_before_the_upstream_call() {
        // FEATURE §27: "A guard rejection in the request phase stops the
        // chain before the upstream call" (DoD
        // `cpt-cf-oagw-dod-plugin-system-execution-order`).  A guard that
        // rejects must short-circuit the request half: the transform phase
        // (and therefore the upstream call) never runs.
        use crate::infra::plugin::transform::REQUEST_ID_HEADER;

        let regs = regs();
        let mut guard = binding(0, REQUIRED_HEADERS_GUARD, None);
        guard.config = json!({ "required_request_headers": "X-Api-Key" });
        let transform = binding(1, REQUEST_ID_TRANSFORM, None);
        let chain = PluginChain::build(&[guard, transform], &regs, Uuid::nil(), None)
            .await
            .expect("builds");

        // Missing the required header → rejected before the transform mints
        // X-Request-ID (no transform-phase side effect ever runs).
        let mut ctx = RequestContext {
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            headers: Headers::new(),
            config: json!({}),
            ..RequestContext::default()
        };
        let outcome = chain.run_request_chain(&mut ctx).await;
        match outcome {
            ChainOutcome::Rejected(rejection) => {
                assert_eq!(rejection.code, "REQUIRED_HEADER_MISSING");
                assert_eq!(rejection.status, 400);
                assert_eq!(rejection.detail, "X-Api-Key", "the configured missing name");
                // The transform phase never ran.
                assert!(
                    !ctx.headers.contains(REQUEST_ID_HEADER),
                    "chain stopped before the transform phase"
                );
            }
            other => panic!("expected a guard rejection, got {other:?}"),
        }

        // With the required header present the same chain proceeds and the
        // request transform mints the correlation id.
        let mut ctx = RequestContext {
            method: "GET".to_owned(),
            path: "/v1/models".to_owned(),
            headers: Headers::new(),
            config: json!({}),
            ..RequestContext::default()
        };
        ctx.headers.insert("X-Api-Key", "secret");
        let outcome = chain.run_request_chain(&mut ctx).await;
        assert!(matches!(outcome, ChainOutcome::Proceed));
        assert!(ctx.headers.contains(REQUEST_ID_HEADER), "transform ran");
    }

    #[tokio::test]
    async fn chain_error_mapping_keeps_503_for_plugin_not_found() {
        let err = PluginChainError::PluginNotFound {
            detail: "x".to_owned(),
        };
        let de = err.to_domain_error();
        assert_eq!(de.status(), 503);
        assert_eq!(
            de.instance(),
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
    }
}
