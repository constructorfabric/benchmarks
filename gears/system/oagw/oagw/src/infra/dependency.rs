//! Platform-dependency resolution (`cpt-cf-oagw-flow-gear-bootstrap`).
//!
//! The `types_registry` SDK client is a gear-level dependency (declared by the
//! gear's `deps` list) and is resolved through the toolkit client hub like the
//! platform-provided `cred_store`, `toolkit-auth` and `tenant-resolver`
//! clients. A dependency that cannot be resolved fails startup with the typed
//! startup error surface (§3) naming the missing dependency, so the failure
//! surfaces at startup and not at request time.

// @cpt-begin:cpt-cf-oagw-dod-dependency-wiring:p1:inst-full
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::ClientHub;
use toolkit_security::DynBearerAuthenticator;
use types_registry_sdk::TypesRegistryClient;

use crate::domain::error::OagwError;

/// Hub key of the credential dependency (`cpt-cf-oagw-contract-cred-store`).
pub const CRED_STORE_DEPENDENCY: &str = "cred_store";
/// Hub key of the toolkit-auth bearer bridge.
pub const TOOLKIT_AUTH_DEPENDENCY: &str = "toolkit-auth";
/// Hub key of the tenant-resolver dependency.
pub const TENANT_RESOLVER_DEPENDENCY: &str = "tenant-resolver";
/// Hub key of the types-registry dependency.
pub const TYPES_REGISTRY_DEPENDENCY: &str = "types_registry";

/// The platform dependencies the gear resolves at startup.
#[derive(Clone)]
pub struct PlatformDependencies {
    /// Types-registry client used for GTS identifier-family provisioning.
    pub types_registry: Arc<dyn TypesRegistryClient>,
    /// Credential store the auth plugins use to resolve `cred://` references.
    pub cred_store: Arc<dyn CredStoreClientV1>,
    /// Toolkit-auth bearer bridge registered by the authn-resolver gear.
    pub bearer_authenticator: Arc<DynBearerAuthenticator>,
    /// Tenant resolver used to scope management resources.
    pub tenant_resolver: Arc<dyn TenantResolverClient>,
}

impl std::fmt::Debug for PlatformDependencies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformDependencies")
            .field("types_registry", &"resolved")
            .field("cred_store", &"resolved")
            .field("bearer_authenticator", &"resolved")
            .field("tenant_resolver", &"resolved")
            .finish()
    }
}

/// Resolves the platform dependencies from the gear context's client hub.
///
/// # Errors
/// Returns `SecretNotFound` for an unresolvable credential dependency and
/// `LinkUnavailable` for an unreachable remote dependency, both naming the
/// missing dependency.
pub fn resolve(hub: &ClientHub) -> Result<PlatformDependencies, OagwError> {
    let types_registry = require_remote(hub, TYPES_REGISTRY_DEPENDENCY)?;
    let cred_store = require_credential(hub, CRED_STORE_DEPENDENCY)?;
    let bearer_authenticator = require_credential(hub, TOOLKIT_AUTH_DEPENDENCY)?;
    let tenant_resolver = require_remote(hub, TENANT_RESOLVER_DEPENDENCY)?;

    Ok(PlatformDependencies {
        types_registry,
        cred_store,
        bearer_authenticator,
        tenant_resolver,
    })
}

/// Resolves a remote platform dependency; absence is a `LinkUnavailable`.
fn require_remote<T: ?Sized + Send + Sync + 'static>(
    hub: &ClientHub,
    dependency: &str,
) -> Result<Arc<T>, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-07
    // A remote dependency the hub cannot resolve fails the init step.
    hub.try_get::<T>().ok_or_else(|| {
        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-08
        // The typed startup error surface names the missing dependency, so the
        // failure surfaces at startup and not at request time.
        OagwError::link_unavailable(format!(
            "oagw.dependencies: required platform dependency '{dependency}' is not available in the client hub"
        ))
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-08
    })
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-07
}

/// Resolves a credential platform dependency; absence is a `SecretNotFound`.
fn require_credential<T: ?Sized + Send + Sync + 'static>(
    hub: &ClientHub,
    dependency: &str,
) -> Result<Arc<T>, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-07
    // A credential dependency the hub cannot resolve fails the init step.
    hub.try_get::<T>().ok_or_else(|| {
        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-08
        // The typed startup error surface names the missing dependency, so the
        // failure surfaces at startup and not at request time.
        OagwError::secret_not_found(format!(
            "oagw.dependencies: required credential dependency '{dependency}' is not available in the client hub"
        ))
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-08
    })
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gb-07
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use types_registry_sdk::testing::MockTypesRegistryClient;

    /// An inert authenticator for the bearer-bridge hub registration.
    struct NullAuthenticator;

    impl toolkit_security::BearerAuthenticator for NullAuthenticator {
        fn authenticate(
            &self,
            _token: &str,
        ) -> impl Future<
            Output = Result<toolkit_security::SecurityContext, toolkit_security::AuthNError>,
        > + Send {
            std::future::ready(Err(toolkit_security::AuthNError::InvalidToken))
        }
    }

    /// An inert tenant resolver.
    struct NullTenantResolver;

    #[async_trait::async_trait]
    impl TenantResolverClient for NullTenantResolver {
        async fn get_tenant(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            id: tenant_resolver_sdk::TenantId,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound { tenant_id: id })
        }

        async fn get_root_tenant(
            &self,
            _ctx: &toolkit_security::SecurityContext,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound {
                tenant_id: tenant_resolver_sdk::TenantId(uuid::Uuid::nil()),
            })
        }

        async fn get_tenants(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _ids: &[tenant_resolver_sdk::TenantId],
            _options: &tenant_resolver_sdk::GetTenantsOptions,
        ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError>
        {
            Ok(Vec::new())
        }

        async fn get_ancestors(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetAncestorsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetAncestorsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            Ok(tenant_resolver_sdk::GetAncestorsResponse {
                tenant: tenant_resolver_sdk::TenantRef {
                    id,
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                ancestors: Vec::new(),
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetDescendantsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetDescendantsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            Ok(tenant_resolver_sdk::GetDescendantsResponse {
                tenant: tenant_resolver_sdk::TenantRef {
                    id,
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                descendants: Vec::new(),
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &toolkit_security::SecurityContext,
            _ancestor_id: tenant_resolver_sdk::TenantId,
            _descendant_id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::IsAncestorOptions,
        ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
            Ok(false)
        }
    }

    /// Builds a hub carrying exactly the platform dependencies asked for, in
    /// the same types the producing gears register them under.
    fn hub(types_registry: bool, cred_store: bool, bearer: bool, tenant: bool) -> ClientHub {
        let hub = ClientHub::default();
        if types_registry {
            hub.register::<dyn TypesRegistryClient>(Arc::new(MockTypesRegistryClient::new()));
        }
        if cred_store {
            hub.register::<dyn CredStoreClientV1>(Arc::new(MockCredStoreClient::empty()));
        }
        if bearer {
            hub.register::<DynBearerAuthenticator>(Arc::new(DynBearerAuthenticator::new(
                NullAuthenticator,
            )));
        }
        if tenant {
            hub.register::<dyn TenantResolverClient>(Arc::new(NullTenantResolver));
        }
        hub
    }

    #[test]
    fn all_four_dependencies_resolve_from_the_hub() {
        let deps = resolve(&hub(true, true, true, true)).unwrap();

        assert_eq!(Arc::strong_count(&deps.types_registry), 1);
        assert_eq!(Arc::strong_count(&deps.cred_store), 1);
        assert_eq!(Arc::strong_count(&deps.bearer_authenticator), 1);
        assert_eq!(Arc::strong_count(&deps.tenant_resolver), 1);
    }

    #[test]
    fn a_missing_credential_dependency_fails_fast_naming_it() {
        let error = resolve(&hub(true, false, true, true)).unwrap_err();

        assert_eq!(error.mapping().variant, "SecretNotFound");
        assert_eq!(error.status(), 500);
        assert!(
            error.detail().contains("'cred_store'"),
            "the missing credential dependency must be named, got: {}",
            error.detail()
        );
    }

    #[test]
    fn a_missing_toolkit_auth_bridge_fails_fast_naming_it() {
        let error = resolve(&hub(true, true, false, true)).unwrap_err();

        assert_eq!(error.mapping().variant, "SecretNotFound");
        assert!(
            error.detail().contains("'toolkit-auth'"),
            "the missing credential dependency must be named, got: {}",
            error.detail()
        );
    }

    #[test]
    fn a_missing_remote_dependency_fails_fast_naming_it() {
        let error = resolve(&hub(true, true, true, false)).unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert!(
            error.detail().contains("'tenant-resolver'"),
            "the missing remote dependency must be named, got: {}",
            error.detail()
        );

        let error = resolve(&hub(false, true, true, true)).unwrap_err();
        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert!(
            error.detail().contains("'types_registry'"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn an_empty_hub_reports_the_first_missing_dependency() {
        let error = resolve(&ClientHub::default()).unwrap_err();
        assert!(
            error.detail().contains("not available in the client hub"),
            "{}",
            error.detail()
        );
    }

    #[test]
    fn dependency_names_are_the_documented_hub_keys() {
        assert_eq!(CRED_STORE_DEPENDENCY, "cred_store");
        assert_eq!(TOOLKIT_AUTH_DEPENDENCY, "toolkit-auth");
        assert_eq!(TENANT_RESOLVER_DEPENDENCY, "tenant-resolver");
        assert_eq!(TYPES_REGISTRY_DEPENDENCY, "types_registry");
    }
}

// @cpt-end:cpt-cf-oagw-dod-dependency-wiring:p1:inst-full
