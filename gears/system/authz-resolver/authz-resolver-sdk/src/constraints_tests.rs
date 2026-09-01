// Created: 2026-04-14 by Constructor Tech
use super::*;
use serde_json::json;
use toolkit_security::pep_properties;

#[test]
fn constraint_serialization_roundtrip() {
    let constraint = Constraint {
        predicates: vec![
            Predicate::In(InPredicate {
                property: pep_properties::OWNER_TENANT_ID.to_owned(),
                values: vec![
                    json!("11111111-1111-1111-1111-111111111111"),
                    json!("22222222-2222-2222-2222-222222222222"),
                ],
            }),
            Predicate::Eq(EqPredicate {
                property: pep_properties::RESOURCE_ID.to_owned(),
                value: json!("33333333-3333-3333-3333-333333333333"),
            }),
        ],
    };

    let json_str = serde_json::to_string(&constraint).unwrap();
    let deserialized: Constraint = serde_json::from_str(&json_str).unwrap();
    assert_eq!(deserialized.predicates.len(), 2);
}

#[test]
fn predicate_tag_serialization() {
    let eq = Predicate::Eq(EqPredicate {
        property: pep_properties::RESOURCE_ID.to_owned(),
        value: json!("00000000-0000-0000-0000-000000000000"),
    });

    let json_str = serde_json::to_string(&eq).unwrap();
    assert!(json_str.contains(r#""op":"eq""#));

    let in_pred = Predicate::In(InPredicate {
        property: pep_properties::OWNER_TENANT_ID.to_owned(),
        values: vec![json!("00000000-0000-0000-0000-000000000000")],
    });

    let json_str = serde_json::to_string(&in_pred).unwrap();
    assert!(json_str.contains(r#""op":"in""#));
}

#[test]
fn in_group_predicate_serialization() {
    let pred = Predicate::InGroup(InGroupPredicate {
        property: pep_properties::RESOURCE_ID.to_owned(),
        group_ids: vec![json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")],
    });

    let json_str = serde_json::to_string(&pred).unwrap();
    assert!(json_str.contains(r#""op":"in_group""#));

    let deserialized: Predicate = serde_json::from_str(&json_str).unwrap();
    assert!(matches!(deserialized, Predicate::InGroup(_)));
}

#[test]
fn in_group_subtree_predicate_serialization() {
    let pred = Predicate::InGroupSubtree(InGroupSubtreePredicate {
        property: pep_properties::RESOURCE_ID.to_owned(),
        ancestor_ids: vec![json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")],
    });

    let json_str = serde_json::to_string(&pred).unwrap();
    assert!(json_str.contains(r#""op":"in_group_subtree""#));

    let deserialized: Predicate = serde_json::from_str(&json_str).unwrap();
    assert!(matches!(deserialized, Predicate::InGroupSubtree(_)));
}

#[test]
fn in_tenant_subtree_predicate_serialization() {
    let pred = Predicate::InTenantSubtree(InTenantSubtreePredicate {
        property: pep_properties::OWNER_TENANT_ID.to_owned(),
        root_tenant_id: json!("cccccccc-cccc-cccc-cccc-cccccccccccc"),
        barrier_mode: BarrierMode::Respect,
        descendant_status: vec![],
    });

    let json_str = serde_json::to_string(&pred).unwrap();
    assert!(json_str.contains(r#""op":"in_tenant_subtree""#));
    assert!(json_str.contains(r#""root_tenant_id":"cccccccc-cccc-cccc-cccc-cccccccccccc""#));

    let deserialized: Predicate = serde_json::from_str(&json_str).unwrap();
    assert!(matches!(deserialized, Predicate::InTenantSubtree(_)));
}

#[test]
fn in_tenant_subtree_predicate_constructor_defaults_to_respect() {
    let pred = InTenantSubtreePredicate::new(
        pep_properties::RESOURCE_ID,
        json!("dddddddd-dddd-dddd-dddd-dddddddddddd"),
    );
    assert_eq!(pred.property, pep_properties::RESOURCE_ID);
    assert_eq!(
        pred.root_tenant_id,
        json!("dddddddd-dddd-dddd-dddd-dddddddddddd")
    );
    assert!(matches!(pred.barrier_mode, BarrierMode::Respect));
}

#[test]
fn in_tenant_subtree_predicate_with_barrier_mode_ignore() {
    let pred = InTenantSubtreePredicate::with_barrier_mode(
        pep_properties::OWNER_TENANT_ID,
        json!("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee"),
        BarrierMode::Ignore,
    );
    assert!(matches!(pred.barrier_mode, BarrierMode::Ignore));
}

#[test]
fn in_tenant_subtree_predicate_deserializes_without_barrier_mode() {
    // Older PDP responses may omit `barrier_mode`. `#[serde(default)]` must
    // honor `BarrierMode`'s `Default` impl (Respect) so we never silently
    // leak across self-managed barriers.
    let json_str = r#"{
        "op": "in_tenant_subtree",
        "property": "owner_tenant_id",
        "root_tenant_id": "ffffffff-ffff-ffff-ffff-ffffffffffff"
    }"#;
    let deserialized: Predicate = serde_json::from_str(json_str).unwrap();
    match deserialized {
        Predicate::InTenantSubtree(p) => {
            assert!(matches!(p.barrier_mode, BarrierMode::Respect));
        }
        other => panic!("unexpected predicate: {other:?}"),
    }
}

#[test]
fn in_tenant_subtree_predicate_deserializes_descendant_status_from_pdp() {
    // PDP JSON that includes descendant_status. Each element must
    // deserialize into the canonical `TenantStatus` enum so the compiler
    // can map it to the SMALLINT encoding when lowering to SQL.
    use tenant_resolver_sdk::TenantStatus;

    let json_str = r#"{
        "op": "in_tenant_subtree",
        "property": "owner_tenant_id",
        "root_tenant_id": "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "descendant_status": ["active", "suspended"]
    }"#;
    let deserialized: Predicate = serde_json::from_str(json_str).unwrap();
    match deserialized {
        Predicate::InTenantSubtree(p) => {
            assert_eq!(
                p.descendant_status,
                vec![TenantStatus::Active, TenantStatus::Suspended]
            );
        }
        other => panic!("unexpected predicate: {other:?}"),
    }
}

#[test]
fn in_tenant_subtree_predicate_rejects_unknown_descendant_status_value() {
    // Unknown status string must fail deserialization (not silently drop)
    // so a typo or stale PDP client doesn't widen a status-constrained
    // request to "all descendants".
    let json_str = r#"{
        "op": "in_tenant_subtree",
        "property": "owner_tenant_id",
        "root_tenant_id": "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "descendant_status": ["pending"]
    }"#;
    let result: Result<Predicate, _> = serde_json::from_str(json_str);
    assert!(
        result.is_err(),
        "unknown status string must fail deserialization, got: {result:?}"
    );
}

#[test]
fn constraint_with_group_predicates_roundtrip() {
    let constraint = Constraint {
        predicates: vec![
            Predicate::In(InPredicate {
                property: pep_properties::OWNER_TENANT_ID.to_owned(),
                values: vec![json!("11111111-1111-1111-1111-111111111111")],
            }),
            Predicate::InGroup(InGroupPredicate {
                property: pep_properties::RESOURCE_ID.to_owned(),
                group_ids: vec![
                    json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
                    json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
                ],
            }),
        ],
    };

    let json_str = serde_json::to_string(&constraint).unwrap();
    let deserialized: Constraint = serde_json::from_str(&json_str).unwrap();
    assert_eq!(deserialized.predicates.len(), 2);
    assert!(matches!(deserialized.predicates[1], Predicate::InGroup(_)));
}
