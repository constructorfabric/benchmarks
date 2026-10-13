use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

use super::restrict_to_subject;

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

#[test]
fn unconstrained_becomes_tenant_and_owner() {
    let (t, u) = ids();
    let s = restrict_to_subject(&AccessScope::allow_all(), t, u);
    assert!(!s.is_unconstrained());
    assert_eq!(s.constraints().len(), 1);
    assert_eq!(s.all_uuid_values_for(pep_properties::OWNER_TENANT_ID), [t]);
    assert_eq!(s.all_uuid_values_for(pep_properties::OWNER_ID), [u]);
}

#[test]
fn deny_all_stays_deny_all() {
    let (t, u) = ids();
    assert!(restrict_to_subject(&AccessScope::deny_all(), t, u).is_deny_all());
}

#[test]
fn foreign_tenant_constraint_is_dropped() {
    let (t, u) = ids();
    let other = Uuid::new_v4();
    let s = restrict_to_subject(&AccessScope::for_tenant(other), t, u);
    assert!(s.is_deny_all());
}

#[test]
fn multi_tenant_constraint_narrows_to_subject_tenant() {
    let (t, u) = ids();
    let s = restrict_to_subject(&AccessScope::for_tenants(vec![Uuid::new_v4(), t]), t, u);
    assert_eq!(s.all_uuid_values_for(pep_properties::OWNER_TENANT_ID), [t]);
    assert_eq!(s.all_uuid_values_for(pep_properties::OWNER_ID), [u]);
}

#[test]
fn constraint_without_tenant_gets_tenant_and_owner() {
    let (t, u) = ids();
    let chat = Uuid::new_v4();
    let s = restrict_to_subject(&AccessScope::for_resource(chat), t, u);
    let c = &s.constraints()[0];
    let props: Vec<&str> = c.filters().iter().map(ScopeFilter::property).collect();
    assert!(props.contains(&pep_properties::RESOURCE_ID));
    assert!(props.contains(&pep_properties::OWNER_TENANT_ID));
    assert!(props.contains(&pep_properties::OWNER_ID));
}

#[test]
fn foreign_owner_constraint_is_dropped() {
    let (t, u) = ids();
    let scope = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
        ScopeFilter::eq(pep_properties::OWNER_ID, Uuid::new_v4()),
    ]));
    assert!(restrict_to_subject(&scope, t, u).is_deny_all());
}
