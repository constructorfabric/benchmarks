//! SSE relay: turns a [`StreamStart`] into an `text/event-stream` response.
//!
//! Events are relayed as soon as the provider task produces them (no
//! buffering). `ping` events are sent only between `stream_started` and the
//! first content event; a transport keep-alive comment is sent every 30 s.
//! When the provider task ends without a terminal event, the relay sends
//! `error{code: "stream_interrupted"}`. Dropping the response stream (client
//! disconnect) drops the cancel guard, which cancels the turn.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use futures::stream;

use crate::domain::billing::codes;
use crate::domain::stream_types::{LiveStream, StreamEvent, StreamStart};

/// Transport keep-alive interval.
pub const KEEPALIVE: Duration = Duration::from_secs(30);

fn to_event(ev: &StreamEvent) -> Event {
    Event::default().event(ev.name()).data(ev.data().to_string())
}

fn ping() -> Event {
    Event::default().event("ping").data("{}")
}

struct RelayState {
    live: LiveStream,
    ping: tokio::time::Interval,
    content_seen: bool,
    done: bool,
}

fn live_stream(live: LiveStream) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let period = Duration::from_secs(live.ping_interval_secs.max(1));
    let mut ping_iv = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ping_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let state = RelayState { live, ping: ping_iv, content_seen: false, done: false };
    stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        let next = if st.content_seen {
            st.live.rx.recv().await
        } else {
            tokio::select! {
                ev = st.live.rx.recv() => ev,
                _ = st.ping.tick() => {
                    return Some((Ok(ping()), st));
                }
            }
        };
        let Some(ev) = next else {
            st.done = true;
            let ev = StreamEvent::error(codes::STREAM_INTERRUPTED, "The stream was interrupted; check the turn status");
            return Some((Ok(to_event(&ev)), st));
        };
        if ev.is_content() {
            st.content_seen = true;
        }
        if ev.is_terminal() {
            st.done = true;
        }
        let out = to_event(&ev);
        Some((Ok(out), st))
    })
}

/// Build the SSE response.
#[must_use]
pub fn sse_response(start: StreamStart) -> Response {
    let keep = KeepAlive::new().interval(KEEPALIVE);
    let mut resp = match start {
        StreamStart::Replay(events) => {
            let s = stream::iter(events.into_iter().map(|e| Ok::<_, Infallible>(to_event(&e))));
            Sse::new(s).keep_alive(keep).into_response()
        }
        StreamStart::Live(live) => Sse::new(live_stream(live)).keep_alive(keep).into_response(),
    };
    resp.headers_mut()
        .insert("x-accel-buffering", http::HeaderValue::from_static("no"));
    resp
}
