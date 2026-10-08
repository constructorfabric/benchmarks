use time::{Duration, OffsetDateTime};

use super::{Clock, FixedClock, SystemClock};

#[test]
fn fixed_clock_is_frozen_until_moved() {
    let t0 = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let clock = FixedClock::new(t0);
    assert_eq!(clock.now(), t0);
    assert_eq!(clock.now(), t0);

    clock.advance(Duration::seconds(30));
    assert_eq!(clock.now(), t0 + Duration::seconds(30));

    let t1 = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    clock.set(t1);
    assert_eq!(clock.now(), t1);
}

#[test]
fn system_clock_is_utc() {
    assert!(SystemClock.now().offset().is_utc());
}
