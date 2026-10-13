#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the leader elector preparation of
//! `cpt-cf-mini-chat-flow-gear-foundation-gear-start` (`inst-if-leader-workers`).

use super::{
    LeaderElector, LeaderRole, NoopLeaderElector, enabled_roles, leader_workers_enabled,
    prepare_elector, start_elector,
};
use crate::config::MiniChatConfig;
use crate::infra::background_tasks::BackgroundTasks;
use tokio_util::sync::CancellationToken;

fn config(watchdog: bool, reaper: bool) -> MiniChatConfig {
    let mut config = MiniChatConfig::default();
    config.orphan_watchdog.enabled = watchdog;
    config.upload_reaper.enabled = reaper;
    config
}

#[test]
fn the_noop_elector_is_always_the_leader() {
    assert!(NoopLeaderElector.is_leader(LeaderRole::OrphanWatchdog));
    assert!(NoopLeaderElector.is_leader(LeaderRole::UploadReaper));
    assert_eq!(NoopLeaderElector.name(), "noop");
}

#[test]
fn no_elector_is_prepared_when_no_leader_worker_is_enabled() {
    assert!(!leader_workers_enabled(&config(false, false)));
    assert!(prepare_elector(&config(false, false)).is_none());
}

#[test]
fn an_elector_is_prepared_when_any_leader_worker_is_enabled() {
    for (watchdog, reaper) in [(true, false), (false, true), (true, true)] {
        let elector = prepare_elector(&config(watchdog, reaper)).expect("elector");
        assert!(elector.is_leader(LeaderRole::OrphanWatchdog));
    }
}

#[test]
fn the_roles_have_their_documented_names() {
    assert_eq!(LeaderRole::OrphanWatchdog.as_str(), "orphan-watchdog");
    assert_eq!(LeaderRole::UploadReaper.as_str(), "upload-reaper");
    assert_eq!(
        LeaderRole::OrphanWatchdog.lease_name(),
        "mini-chat-orphan-watchdog"
    );
    assert_eq!(
        LeaderRole::UploadReaper.lease_name(),
        "mini-chat-upload-reaper"
    );
    assert_eq!(LeaderRole::ALL.len(), 2);
}

#[test]
fn the_enabled_roles_follow_the_configuration() {
    assert_eq!(
        enabled_roles(&config(true, false)),
        vec![LeaderRole::OrphanWatchdog]
    );
    assert_eq!(
        enabled_roles(&config(false, true)),
        vec![LeaderRole::UploadReaper]
    );
    assert_eq!(
        enabled_roles(&config(true, true)),
        vec![LeaderRole::OrphanWatchdog, LeaderRole::UploadReaper]
    );
    assert!(enabled_roles(&config(false, false)).is_empty());
}

#[tokio::test]
async fn no_elector_is_started_when_no_leader_worker_is_enabled() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());

    let elector = start_elector(&config(false, false), &tasks).await.unwrap();

    assert!(elector.is_none());
    assert!(tasks.running().is_empty());
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_build_without_k8s_starts_the_noop_elector_for_every_role() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());

    let elector = start_elector(&config(true, true), &tasks)
        .await
        .unwrap()
        .expect("an elector");

    assert_eq!(elector.name(), "noop");
    for role in LeaderRole::ALL {
        assert!(elector.is_leader(role), "{role:?}");
    }
    assert!(tasks.running().is_empty(), "no renew task without k8s");
}
