//! Orphan / broken-parent classifier.
//!
//! Walks the tenants and emits two categories:
//!
//! * [`IntegrityCategory::OrphanedChild`] — `parent_id` references a
//!   tenant that is absent from the snapshot.
//! * [`IntegrityCategory::BrokenParentReference`] — `parent_id` resolves
//!   to a tenant whose `status = Deleted` while the child itself is
//!   still SDK-visible (status `<>` `Deleted`). The orphan flavour above
//!   subsumes the missing-parent case so the two categories are
//!   mutually exclusive on a given child row.

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};
use crate::domain::tenant::model::TenantStatus;

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    let mut out = Vec::new();
    for t in snap.tenants() {
        let Some(parent_id) = t.parent_id else {
            continue;
        };
        match snap.tenant(parent_id) {
            None => out.push(Violation {
                category: IntegrityCategory::OrphanedChild,
                tenant_id: Some(t.id),
                details: format!("parent {parent_id} missing for tenant {tid}", tid = t.id),
            }),
            Some(parent) => {
                if parent.status == TenantStatus::Deleted && t.status != TenantStatus::Deleted {
                    let status = t.status.as_smallint();
                    out.push(Violation {
                        category: IntegrityCategory::BrokenParentReference,
                        tenant_id: Some(t.id),
                        details: format!(
                            "tenant {tid} is status={status} but parent {parent_id} is Deleted",
                            tid = t.id
                        ),
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "orphan_tests.rs"]
mod orphan_tests;
