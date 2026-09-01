// Created: 2026-04-20 by Constructor Tech
use super::*;
use tenant_resolver_sdk::TenantStatus;
use uuid::Uuid;

const TENANT_A: &str = "11111111-1111-1111-1111-111111111111";
const TENANT_B: &str = "22222222-2222-2222-2222-222222222222";
const TENANT_C: &str = "33333333-3333-3333-3333-333333333333";

fn tenant(id: &str, parent: Option<&str>) -> TenantConfig {
    TenantConfig {
        id: Uuid::parse_str(id).unwrap(),
        name: id.to_owned(),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: parent.map(|p| Uuid::parse_str(p).unwrap()),
        self_managed: false,
    }
}

#[test]
fn validate_accepts_single_root_with_children() {
    let cfg = StaticTrPluginConfig {
        tenants: vec![
            tenant(TENANT_A, None),
            tenant(TENANT_B, Some(TENANT_A)),
            tenant(TENANT_C, Some(TENANT_B)),
        ],
        ..Default::default()
    };
    cfg.validate().expect("valid single-root tree should pass");
}

#[test]
fn validate_accepts_single_root_alone() {
    let cfg = StaticTrPluginConfig {
        tenants: vec![tenant(TENANT_A, None)],
        ..Default::default()
    };
    cfg.validate().expect("lone root should pass");
}

#[test]
fn validate_rejects_zero_roots() {
    let cfg = StaticTrPluginConfig {
        tenants: Vec::new(),
        ..Default::default()
    };
    let err = cfg.validate().expect_err("empty config must fail");
    assert!(err.to_string().contains("no root tenant"));
}

#[test]
fn validate_rejects_multiple_roots() {
    let cfg = StaticTrPluginConfig {
        tenants: vec![tenant(TENANT_A, None), tenant(TENANT_B, None)],
        ..Default::default()
    };
    let err = cfg.validate().expect_err("two roots must fail");
    assert!(err.to_string().contains("2 root tenants"));
}

#[test]
fn validate_rejects_dangling_parent_reference() {
    let cfg = StaticTrPluginConfig {
        tenants: vec![tenant(TENANT_A, None), tenant(TENANT_B, Some(TENANT_C))],
        ..Default::default()
    };
    let err = cfg.validate().expect_err("dangling parent must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("invalid tenant hierarchy configuration"));
}

#[test]
fn validate_rejects_duplicate_ids() {
    let cfg = StaticTrPluginConfig {
        tenants: vec![
            tenant(TENANT_A, None),
            // Same id, different parent — HashMap would silently drop this
            // without validation.
            tenant(TENANT_A, Some(TENANT_A)),
        ],
        ..Default::default()
    };
    let err = cfg.validate().expect_err("duplicate ids must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("duplicate tenant id"), "got: {msg}");
}

#[test]
fn validate_rejects_self_referential_parent() {
    let cfg = StaticTrPluginConfig {
        tenants: vec![tenant(TENANT_A, None), tenant(TENANT_B, Some(TENANT_B))],
        ..Default::default()
    };
    let err = cfg
        .validate()
        .expect_err("self-referential parent must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("lists itself as parent_id"), "got: {msg}");
}

#[test]
fn validate_rejects_disconnected_cycle() {
    // C is the unique root; A and B form a two-node cycle that never
    // reaches C. The single-root / duplicate / self-ref / dangling checks
    // all pass — only the reachability check catches this.
    let cfg = StaticTrPluginConfig {
        tenants: vec![
            tenant(TENANT_C, None),
            tenant(TENANT_A, Some(TENANT_B)),
            tenant(TENANT_B, Some(TENANT_A)),
        ],
        ..Default::default()
    };
    let err = cfg.validate().expect_err("disconnected cycle must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("parent_id cycle") && msg.contains("does not descend from root"),
        "got: {msg}"
    );
}
