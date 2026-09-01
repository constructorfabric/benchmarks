// Created: 2026-04-29 by Constructor Tech
//! Shared test helpers for `tr-authz-plugin`.
//!
//! Consolidates the previously-duplicated `HierarchyMock`, `FailingOnDescendants`,
//! and `EmptyTr` mock implementations into a single configurable `MockTr` so
//! that the trait-method skeleton is implemented once.

#![cfg(test)]
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::service::Service;

/// Configurable `TenantResolverClient` mock used by both `service_tests` and
/// `client_tests`.
///
/// Behavior is selected via the field configuration:
///
/// - `parent` empty → `EmptyTr`-style:
///     * `get_tenant` and `get_root_tenant` return `TenantNotFound`.
///     * `get_tenants` returns an empty vector.
///     * `get_descendants` returns an empty descendant list.
///     * `is_ancestor` returns `false` (no relationships).
///     * `get_ancestors` is the one exception: it returns the requested id
///       wrapped in a synthetic active `TenantRef` with an empty ancestor
///       chain. The legacy `EmptyTr` did the same; treating it as
///       "not found" would break the AuthZ-plugin code path that walks
///       the ancestor chain to find a tenant root.
/// - `parent` populated → `HierarchyMock`-style: `get_descendants` and
///   `is_ancestor` traverse the in-memory parent map.
/// - `descendants_error == true` → `FailingOnDescendants`-style: every
///   `get_descendants` call returns `TenantResolverError::Internal`.
pub struct MockTr {
    pub all: Vec<Uuid>,
    pub parent: HashMap<Uuid, Option<Uuid>>,
    pub descendants_error: bool,
}

impl MockTr {
    /// All trait methods return "not found" / empty. Matches the legacy
    /// `EmptyTr` mock used by `client_tests`.
    pub fn empty() -> Self {
        Self {
            all: Vec::new(),
            parent: HashMap::new(),
            descendants_error: false,
        }
    }

    /// Hierarchy traversal mock. Tests pass an explicit parent map.
    pub fn with_hierarchy(all: Vec<Uuid>, parent: HashMap<Uuid, Option<Uuid>>) -> Self {
        Self {
            all,
            parent,
            descendants_error: false,
        }
    }

    /// Mock that always fails `get_descendants` with `Internal`. Matches the
    /// legacy `FailingOnDescendants` mock used by `r8_deny_on_tr_error`.
    pub fn failing_descendants() -> Self {
        Self {
            all: Vec::new(),
            parent: HashMap::new(),
            descendants_error: true,
        }
    }

    fn is_ancestor_of(&self, anc: Uuid, desc: Uuid) -> bool {
        let mut cur = Some(desc);
        while let Some(c) = cur {
            match self.parent.get(&c).copied().flatten() {
                Some(p) if p == anc => return true,
                Some(p) => cur = Some(p),
                None => return false,
            }
        }
        false
    }

    fn collect_descendants(&self, root: Uuid) -> Vec<Uuid> {
        self.all
            .iter()
            .copied()
            .filter(|&t| t != root && self.is_ancestor_of(root, t))
            .collect()
    }
}

#[async_trait]
impl TenantResolverClient for MockTr {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::TenantNotFound { tenant_id: id })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::TenantNotFound {
            tenant_id: TenantId(Uuid::nil()),
        })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(vec![])
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        Ok(GetAncestorsResponse {
            tenant: TenantRef {
                id,
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: None,
                self_managed: false,
            },
            ancestors: vec![],
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        if self.descendants_error {
            return Err(TenantResolverError::Internal(
                "simulated TR failure".to_owned(),
            ));
        }
        let tenant_ref = TenantRef {
            id,
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: self.parent.get(&id.0).copied().flatten().map(TenantId),
            self_managed: false,
        };
        let descendants = self
            .collect_descendants(id.0)
            .into_iter()
            .map(|d| TenantRef {
                id: TenantId(d),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: self.parent.get(&d).copied().flatten().map(TenantId),
                self_managed: false,
            })
            .collect();
        Ok(GetDescendantsResponse {
            tenant: tenant_ref,
            descendants,
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(self.is_ancestor_of(ancestor_id.0, descendant_id.0))
    }
}

/// Build the canonical `r → t1 → t2 → {t3, t4}` hierarchy and wrap it in a
/// ready-to-use `Service`. Used by every R-rule test in `service_tests`.
pub fn setup_svc() -> (Service, [Uuid; 5]) {
    let r = Uuid::now_v7();
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let t3 = Uuid::now_v7();
    let t4 = Uuid::now_v7();
    let parent: HashMap<Uuid, Option<Uuid>> = [
        (r, None),
        (t1, Some(r)),
        (t2, Some(t1)),
        (t3, Some(t2)),
        (t4, Some(t2)),
    ]
    .into_iter()
    .collect();
    let mock = MockTr::with_hierarchy(vec![r, t1, t2, t3, t4], parent);
    (Service::new(Arc::new(mock)), [r, t1, t2, t3, t4])
}
