//! SSE writer: `stream_started`, pings before content, relayed events, and
//! `stream_interrupted` when the provider task ends without a terminal event.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::dto::{StreamStartedData, sse_parts};
use crate::domain::stream::{StreamEvent, StreamStart};

/// Cancels the turn when the response body is dropped (client disconnect).
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct State {
    pending: VecDeque<Event>,
    rx: Option<mpsc::Receiver<StreamEvent>>,
    _guard: Option<CancelOnDrop>,
    ping: Duration,
    content_started: bool,
    finished: bool,
}

fn event(name: &str, data: String) -> Event {
    Event::default().event(name).data(data)
}

fn to_event(ev: StreamEvent) -> Event {
    let (name, data) = sse_parts(ev);
    event(name, data)
}

fn into_stream(start: StreamStart, ping: Duration) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let mut pending = VecDeque::new();
    let state = match start {
        StreamStart::Replay { header, events } => {
            pending.push_back(event(
                "stream_started",
                serde_json::to_string(&StreamStartedData::from(&header)).unwrap_or_default(),
            ));
            for e in events {
                pending.push_back(to_event(e));
            }
            State { pending, rx: None, _guard: None, ping, content_started: true, finished: true }
        }
        StreamStart::Live(live) => {
            pending.push_back(event(
                "stream_started",
                serde_json::to_string(&StreamStartedData::from(&live.header)).unwrap_or_default(),
            ));
            State {
                pending,
                rx: Some(live.rx),
                _guard: Some(CancelOnDrop(live.cancel)),
                ping,
                content_started: false,
                finished: false,
            }
        }
    };
    futures::stream::unfold(state, |mut st| async move {
        if let Some(e) = st.pending.pop_front() {
            return Some((Ok(e), st));
        }
        if st.finished {
            return None;
        }
        let rx = st.rx.as_mut()?;
        let next = if st.content_started {
            rx.recv().await.map(Some)
        } else {
            tokio::select! {
                ev = rx.recv() => ev.map(Some),
                () = tokio::time::sleep(st.ping) => Some(None),
            }
        };
        match next {
            Some(Some(ev)) => {
                if ev.is_content() {
                    st.content_started = true;
                }
                if ev.is_terminal() {
                    st.finished = true;
                }
                Some((Ok(to_event(ev)), st))
            }
            Some(None) => Some((Ok(event("ping", "{}".to_owned())), st)),
            None => {
                st.finished = true;
                Some((
                    Ok(event(
                        "error",
                        serde_json::json!({"code": "stream_interrupted", "message": "The stream was interrupted"}).to_string(),
                    )),
                    st,
                ))
            }
        }
    })
}

/// Builds the SSE response of a stream start.
#[must_use]
pub fn sse_response(start: StreamStart, ping: Duration) -> Response {
    Sse::new(into_stream(start, ping))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}
