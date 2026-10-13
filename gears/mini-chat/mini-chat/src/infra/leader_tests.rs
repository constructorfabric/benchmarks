use std::time::Duration;

use super::*;

const D: Duration = Duration::from_secs(15);

#[test]
fn missing_or_empty_holder_is_acquired() {
    assert_eq!(decide(None, None, D, "me", 100.0), LeaseAction::Acquire);
    assert_eq!(decide(Some(""), Some(99.0), D, "me", 100.0), LeaseAction::Acquire);
}

#[test]
fn own_lease_is_renewed() {
    assert_eq!(decide(Some("me"), Some(99.0), D, "me", 100.0), LeaseAction::Renew);
    // Even when expired: the holder renews its own lease.
    assert_eq!(decide(Some("me"), Some(1.0), D, "me", 100.0), LeaseAction::Renew);
}

#[test]
fn foreign_lease_is_followed_until_it_expires() {
    assert_eq!(decide(Some("other"), Some(90.0), D, "me", 100.0), LeaseAction::Follow);
    assert_eq!(decide(Some("other"), Some(84.0), D, "me", 100.0), LeaseAction::Acquire);
    assert_eq!(decide(Some("other"), None, D, "me", 100.0), LeaseAction::Acquire);
}

#[test]
fn lease_names_use_the_hardcoded_prefix() {
    assert_eq!(lease_name(ROLE_ORPHAN_WATCHDOG), "mini-chat-orphan-watchdog");
    assert_eq!(lease_name(ROLE_UPLOAD_REAPER), "mini-chat-upload-reaper");
}
