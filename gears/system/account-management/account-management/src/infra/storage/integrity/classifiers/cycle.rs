//! Cycle classifier — detects `parent_id` cycles using iterative DFS
//! over the tenants graph.
//!
//! Outputs one [`IntegrityCategory::Cycle`] violation per node that
//! participates in a cycle. The walk is `O(V + E)` and uses an explicit
//! `visited` set + `recstack` to keep recursion-free behaviour for
//! arbitrary input shapes (including self-loops where `parent_id = id`).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    // Adjacency: child -> parent (one outgoing edge per node).
    let mut parent_of: HashMap<Uuid, Uuid> = HashMap::with_capacity(snap.tenants().len());
    for t in snap.tenants() {
        if let Some(p) = t.parent_id {
            parent_of.insert(t.id, p);
        }
    }

    let mut visited: HashSet<Uuid> = HashSet::new();
    let mut on_cycle: HashSet<Uuid> = HashSet::new();

    // Iterative walk: from each unvisited node, follow parent edges,
    // tracking the path. If we reach a node already on the current path,
    // every node from that point onward is on a cycle.
    for t in snap.tenants() {
        if visited.contains(&t.id) {
            continue;
        }
        let mut path: Vec<Uuid> = Vec::new();
        let mut path_set: HashSet<Uuid> = HashSet::new();
        let mut cursor: Option<Uuid> = Some(t.id);
        while let Some(node) = cursor {
            if visited.contains(&node) {
                // The frontier already explored this node; everything in
                // `path` exits without forming a new cycle here.
                break;
            }
            if path_set.contains(&node) {
                // Walked back into our own path → mark every member from
                // the loop entry point onward as cyclic.
                if let Some(idx) = path.iter().position(|p| *p == node) {
                    for n in &path[idx..] {
                        on_cycle.insert(*n);
                    }
                }
                break;
            }
            path.push(node);
            path_set.insert(node);
            cursor = parent_of.get(&node).copied();
        }
        for n in &path {
            visited.insert(*n);
        }
    }

    let mut out = Vec::with_capacity(on_cycle.len());
    for t in snap.tenants() {
        if !on_cycle.contains(&t.id) {
            continue;
        }
        // Self-loop tenants list themselves as the parent of the loop;
        // every other on-cycle node has its `parent_id` populated.
        let parent_id = t.parent_id.unwrap_or(t.id);
        out.push(Violation {
            category: IntegrityCategory::Cycle,
            tenant_id: Some(t.id),
            details: format!("cycle detected at parent {parent_id}"),
        });
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::tenant::model::TenantStatus;
    use crate::infra::storage::integrity::snapshot::TenantSnap;

    fn t(id: u128, parent: Option<u128>) -> TenantSnap {
        TenantSnap {
            id: Uuid::from_u128(id),
            parent_id: parent.map(Uuid::from_u128),
            status: TenantStatus::Active,
            depth: 0,
            self_managed: false,
        }
    }

    #[test]
    fn empty_input_yields_no_violations() {
        let snap = Snapshot::new(vec![], vec![]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn acyclic_tree_yields_no_violations() {
        let snap = Snapshot::new(
            vec![t(1, None), t(2, Some(1)), t(3, Some(1)), t(4, Some(2))],
            vec![],
        );
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn self_loop_is_detected() {
        let snap = Snapshot::new(vec![t(1, Some(1))], vec![]);
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, IntegrityCategory::Cycle);
        assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(1)));
    }

    #[test]
    fn two_cycle_is_detected() {
        let snap = Snapshot::new(vec![t(1, Some(2)), t(2, Some(1))], vec![]);
        let v = classify(&snap);
        assert_eq!(v.len(), 2);
        let ids: Vec<_> = v.iter().filter_map(|x| x.tenant_id).collect();
        assert!(ids.contains(&Uuid::from_u128(1)));
        assert!(ids.contains(&Uuid::from_u128(2)));
    }

    #[test]
    fn three_cycle_is_detected() {
        let snap = Snapshot::new(vec![t(1, Some(2)), t(2, Some(3)), t(3, Some(1))], vec![]);
        let v = classify(&snap);
        assert_eq!(v.len(), 3);
        for n in &v {
            assert_eq!(n.category, IntegrityCategory::Cycle);
        }
    }

    #[test]
    fn cycle_touching_acyclic_branch_only_flags_loop_members() {
        // 1 -> 2 -> 3 -> 1 (cycle); 4 -> 2 (dangling tail attached).
        let snap = Snapshot::new(
            vec![t(1, Some(2)), t(2, Some(3)), t(3, Some(1)), t(4, Some(2))],
            vec![],
        );
        let v = classify(&snap);
        assert_eq!(v.len(), 3, "only the 3 cycle members should be reported");
        let ids: std::collections::HashSet<_> = v.iter().filter_map(|x| x.tenant_id).collect();
        assert!(ids.contains(&Uuid::from_u128(1)));
        assert!(ids.contains(&Uuid::from_u128(2)));
        assert!(ids.contains(&Uuid::from_u128(3)));
        assert!(!ids.contains(&Uuid::from_u128(4)));
    }
}
