//! Model catalog visibility and chat-model resolution (DESIGN section 3.3 "Models API" and
//! "Create Chat", PRD `cpt-cf-mini-chat-fr-model-selection`).

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::infra::gateways::policy::PolicyGateway;

/// Reads the model catalog from the policy plugin on every call (no local snapshot cache,
/// ADR-0008).
#[domain_model]
pub struct ModelService {
    policy: Arc<dyn PolicyGateway>,
    authz: Arc<Authz>,
}

/// Default model of a new chat: the first enabled entry with `preference.is_default`, else the
/// first enabled entry, in catalog order (tier is not considered).
fn default_model(catalog: &[ModelCatalogEntry]) -> Option<&ModelCatalogEntry> {
    catalog
        .iter()
        .find(|m| m.enabled && m.preference.as_ref().is_some_and(|p| p.is_default))
        .or_else(|| catalog.iter().find(|m| m.enabled))
}

impl ModelService {
    #[must_use]
    pub fn new(policy: Arc<dyn PolicyGateway>, authz: Arc<Authz>) -> Self {
        Self { policy, authz }
    }

    async fn catalog(&self, ctx: &SecurityContext) -> Result<PolicySnapshot, DomainError> {
        self.policy.current_snapshot(ctx.subject_id()).await
    }

    /// Enabled models of the caller's current policy snapshot, in catalog order (PDP action
    /// `list` on the model resource).
    ///
    /// # Errors
    /// PDP denial / failure, policy plugin failure.
    pub async fn list_visible(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.authz.model_permission(ctx, "list").await?;
        let snapshot = self.catalog(ctx).await?;
        Ok(snapshot
            .model_catalog
            .into_iter()
            .filter(|m| m.enabled)
            .collect())
    }

    /// One enabled model (PDP action `read` on the model resource).
    ///
    /// # Errors
    /// `ModelNotFound` when the model is disabled or not in the catalog; PDP / plugin failures.
    pub async fn get_visible(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<ModelCatalogEntry, DomainError> {
        self.authz.model_permission(ctx, "read").await?;
        let snapshot = self.catalog(ctx).await?;
        snapshot
            .model_catalog
            .into_iter()
            .find(|m| m.enabled && m.id == id)
            .ok_or_else(|| DomainError::ModelNotFound { id: id.to_owned() })
    }

    /// Model of a new chat: the requested model when it is enabled, otherwise (nothing requested)
    /// the default model. No PDP call: chat creation is authorized by the caller.
    ///
    /// # Errors
    /// `InvalidModel` when the requested model is unknown or disabled, or no model is enabled.
    pub async fn resolve_for_create(
        &self,
        ctx: &SecurityContext,
        requested: Option<&str>,
    ) -> Result<ModelCatalogEntry, DomainError> {
        let snapshot = self.catalog(ctx).await?;
        let found = match requested {
            Some(id) => snapshot
                .model_catalog
                .iter()
                .find(|m| m.enabled && m.id == id),
            None => default_model(&snapshot.model_catalog),
        };
        found.cloned().ok_or(DomainError::InvalidModel)
    }

    /// Model of an existing chat, without the enabled filter (a disabled model is downgraded by
    /// the quota cascade later), with the snapshot it was read from.
    ///
    /// # Errors
    /// `InvalidModel` when the model was removed from the catalog; plugin failures.
    pub async fn resolve_chat_model(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> Result<(PolicySnapshot, ModelCatalogEntry), DomainError> {
        let snapshot = self.catalog(ctx).await?;
        let model = snapshot
            .model_catalog
            .iter()
            .find(|m| m.id == model_id)
            .cloned()
            .ok_or(DomainError::InvalidModel)?;
        Ok((snapshot, model))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use authz_resolver_sdk::PolicyEnforcer;
    use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelPreference, TierLimits};
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::*;
    use crate::infra::gateways::policy::DirectPolicyGateway;
    use crate::test_support::catalog::{premium_model, standard_model};
    use crate::test_support::pdp::{FakePdp, PdpMode};
    use crate::test_support::plugins::RecordingPolicy;

    fn service(catalog: Vec<ModelCatalogEntry>) -> ModelService {
        let limits = TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 1,
        };
        let switches = KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: false,
            disable_web_search: false,
            disable_file_search: false,
            disable_images: false,
            disable_code_interpreter: false,
        };
        let policy = Arc::new(RecordingPolicy::new(catalog, switches, limits, limits));
        ModelService::new(
            Arc::new(DirectPolicyGateway(policy)),
            Arc::new(Authz::new(PolicyEnforcer::new(Arc::new(FakePdp::new(
                PdpMode::TenantConstraint,
            ))))),
        )
    }

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("ctx")
    }

    fn disabled(mut m: ModelCatalogEntry) -> ModelCatalogEntry {
        m.enabled = false;
        m
    }

    fn default_pref(mut m: ModelCatalogEntry) -> ModelCatalogEntry {
        m.preference = Some(ModelPreference {
            is_default: true,
            sort_order: 0,
        });
        m
    }

    async fn default_id(catalog: Vec<ModelCatalogEntry>) -> Result<String, DomainError> {
        service(catalog)
            .resolve_for_create(&ctx(), None)
            .await
            .map(|m| m.id)
    }

    #[tokio::test]
    async fn default_model_algorithm() {
        // First enabled `is_default` wins over earlier enabled models; a disabled default is
        // skipped; tier is not considered.
        assert_eq!(
            default_id(vec![
                disabled(default_pref(premium_model("d0"))),
                standard_model("s1"),
                default_pref(standard_model("s2")),
                default_pref(premium_model("p3")),
            ])
            .await
            .unwrap(),
            "s2"
        );
        // No enabled `is_default`: first enabled model.
        assert_eq!(
            default_id(vec![
                disabled(default_pref(standard_model("d0"))),
                premium_model("p1"),
                standard_model("s2"),
            ])
            .await
            .unwrap(),
            "p1"
        );
        // No enabled model at all.
        let err = default_id(vec![disabled(standard_model("d0"))])
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidModel), "{err:?}");
        let err = default_id(vec![]).await.unwrap_err();
        assert!(matches!(err, DomainError::InvalidModel), "{err:?}");

        let svc = service(vec![disabled(premium_model("d0")), standard_model("s1")]);
        let ctx = ctx();
        // Requested model: must be enabled and present.
        assert_eq!(
            svc.resolve_for_create(&ctx, Some("s1")).await.unwrap().id,
            "s1"
        );
        for requested in ["d0", "nope"] {
            let err = svc
                .resolve_for_create(&ctx, Some(requested))
                .await
                .unwrap_err();
            assert!(
                matches!(err, DomainError::InvalidModel),
                "{requested}: {err:?}"
            );
        }

        // An existing chat resolves its model without the enabled filter.
        let (snapshot, model) = svc.resolve_chat_model(&ctx, "d0").await.unwrap();
        assert_eq!(model.id, "d0");
        assert!(!model.enabled);
        assert_eq!(snapshot.policy_version, 1);
        assert_eq!(snapshot.model_catalog.len(), 2);
        let err = svc.resolve_chat_model(&ctx, "removed").await.unwrap_err();
        assert!(matches!(err, DomainError::InvalidModel), "{err:?}");
    }
}
