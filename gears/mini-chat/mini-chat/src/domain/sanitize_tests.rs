use super::*;

#[test]
fn every_rule_pattern_compiles() {
    assert_eq!(COMPILED.len(), RULES.len());
}

#[test]
fn client_messages_are_sanitized_and_bounded() {
    let long = format!(
        "resp_abcdef123456 {}",
        "x".repeat(2 * MAX_CLIENT_MESSAGE_CHARS)
    );
    let out = client_message(&long);
    assert!(out.starts_with("[provider_id] "), "{out}");
    assert_eq!(out.chars().count(), MAX_CLIENT_MESSAGE_CHARS);
}

#[test]
fn dashes_and_underscores_inside_a_key_belong_to_it() {
    assert_eq!(
        sanitize_provider_message("key sk-proj-AbCdEf_123456789 rejected"),
        "key [credential] rejected"
    );
    assert_eq!(
        sanitize_provider_message("a task-based approach"),
        "a task-based approach"
    );
}
