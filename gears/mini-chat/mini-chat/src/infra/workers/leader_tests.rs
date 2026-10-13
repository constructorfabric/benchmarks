use super::{
    LeaderElector, NoopElector, ORPHAN_WATCHDOG_ROLE, UPLOAD_REAPER_ROLE, default_elector,
};

#[test]
fn noop_elector_is_always_leader() {
    assert!(NoopElector.is_leader(ORPHAN_WATCHDOG_ROLE));
    assert!(NoopElector.is_leader(UPLOAD_REAPER_ROLE));
}

#[test]
fn default_elector_leads_every_role() {
    let e = default_elector();
    assert!(e.is_leader(ORPHAN_WATCHDOG_ROLE));
    assert!(e.is_leader(UPLOAD_REAPER_ROLE));
}
