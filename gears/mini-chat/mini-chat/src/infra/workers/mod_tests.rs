use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::leader::{LeaderElector, NoopElector};
use super::run_periodic;

struct Follower;

impl LeaderElector for Follower {
    fn is_leader(&self, _role: &str) -> bool {
        false
    }
}

async fn scans_within(elector: Arc<dyn LeaderElector>, window: Duration) -> u32 {
    let count = Arc::new(AtomicU32::new(0));
    let cancel = CancellationToken::new();
    let task = {
        let (count, cancel) = (Arc::clone(&count), cancel.clone());
        tokio::spawn(run_periodic(
            "role",
            Duration::from_secs(60),
            elector,
            cancel,
            move || {
                let count = Arc::clone(&count);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            },
        ))
    };
    tokio::time::sleep(window).await;
    cancel.cancel();
    task.await.unwrap();
    count.load(Ordering::SeqCst)
}

#[tokio::test(start_paused = true)]
async fn leader_scans_at_start_and_every_interval() {
    // t = 0, 60, 120 within 150 s.
    assert_eq!(
        scans_within(Arc::new(NoopElector), Duration::from_secs(150)).await,
        3
    );
}

#[tokio::test(start_paused = true)]
async fn follower_never_scans() {
    assert_eq!(
        scans_within(Arc::new(Follower), Duration::from_secs(150)).await,
        0
    );
}
