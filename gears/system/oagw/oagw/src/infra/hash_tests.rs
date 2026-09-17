use serde_json::json;

use super::stable_hash;

#[test]
fn equal_documents_hash_identically() {
    let left = json!({"alpha": 1, "beta": "two"});
    let right = json!({"beta": "two", "alpha": 1});
    assert_eq!(stable_hash(&left), stable_hash(&right));
}

#[test]
fn key_order_does_not_matter_nested() {
    let left = json!({"outer": {"z": [1, 2], "a": true}, "k": null});
    let right = json!({"k": null, "outer": {"a": true, "z": [1, 2]}});
    assert_eq!(stable_hash(&left), stable_hash(&right));
}

#[test]
fn different_values_hash_differently() {
    let left = json!({"alpha": 1});
    let right = json!({"alpha": 2});
    assert_ne!(stable_hash(&left), stable_hash(&right));
}

#[test]
fn array_order_is_significant() {
    let left = json!([1, 2, 3]);
    let right = json!([3, 2, 1]);
    assert_ne!(stable_hash(&left), stable_hash(&right));
}

#[test]
fn scalar_kinds_are_distinguished() {
    assert_ne!(stable_hash(&json!(1)), stable_hash(&json!("1")));
    assert_ne!(stable_hash(&json!(null)), stable_hash(&json!(false)));
    assert_ne!(stable_hash(&json!({})), stable_hash(&json!([])));
}

#[test]
fn digest_is_deterministic_across_calls() {
    let value = json!({"client_id_ref": "cred://id", "scopes": ["a", "b"]});
    let first = stable_hash(&value);
    let second = stable_hash(&value);
    assert_eq!(first, second);
}
