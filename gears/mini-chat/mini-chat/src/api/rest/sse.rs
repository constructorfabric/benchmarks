//! SSE relay: turns the provider task's channel into the public SSE stream.
//!
//! - `ping` only between `stream_started` and the first `delta` / `tool`;
//! - axum comment keep-alive every 30 s;
//! - `error{stream_interrupted}` when the channel closes without a terminal
//!   event;
//! - dropping the response (client disconnect) cancels the turn.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::dto::sse_parts;
use crate::service::stream::{StreamEvent, StreamStart};

/// Cancels the turn when the relay is dropped before the terminal event.
struct CancelGuard {
    token: Option<CancellationToken>,
}

impl CancelGuard {
    fn disarm(&mut self) {
        self.token = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(t) = self.token.take() {
            tracing::debug!("SSE client disconnected; cancelling the turn");
            t.cancel();
        }
    }
}

enum Item {
    Ev(StreamEvent),
    Ping,
}

struct Relay {
    pending: VecDeque<StreamEvent>,
    rx: Option<mpsc::Receiver<StreamEvent>>,
    guard: CancelGuard,
    content_started: bool,
    finished: bool,
    ping_every: Duration,
}

fn to_event(item: Item) -> Event {
    match item {
        Item::Ping => Event::default().event("ping").data("{}"),
        Item::Ev(ev) => {
            let (name, data) = sse_parts(ev);
            Event::default().event(name).data(data.to_string())
        }
    }
}

impl Relay {
    async fn next(&mut self) -> Option<Item> {
        if self.finished {
            return None;
        }
        let ev = if let Some(ev) = self.pending.pop_front() {
            ev
        } else {
            let rx = self.rx.as_mut()?;
            let received = if self.content_started {
                rx.recv().await
            } else {
                tokio::select! {
                    r = rx.recv() => r,
                    () = tokio::time::sleep(self.ping_every) => return Some(Item::Ping),
                }
            };
            match received {
                Some(ev) => ev,
                None => StreamEvent::Error {
                    code: "stream_interrupted".into(),
                    message: "The stream ended unexpectedly; check the turn status".into(),
                },
            }
        };
        if ev.is_content() {
            self.content_started = true;
        }
        if ev.is_terminal() {
            self.finished = true;
            self.guard.disarm();
        }
        Some(Item::Ev(ev))
    }
}

fn relay_stream(relay: Relay) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    futures::stream::unfold(relay, |mut r| async move {
        let item = r.next().await?;
        Some((Ok(to_event(item)), r))
    })
}

/// Build the SSE response of a started stream.
#[must_use]
pub fn sse_response(start: StreamStart, ping_every: Duration) -> Response {
    let relay = match start {
        StreamStart::Replay(events) => Relay {
            pending: events.into(),
            rx: None,
            guard: CancelGuard { token: None },
            content_started: true,
            finished: false,
            ping_every,
        },
        StreamStart::Live {
            started,
            rx,
            cancel,
        } => Relay {
            pending: VecDeque::from([started]),
            rx: Some(rx),
            guard: CancelGuard {
                token: Some(cancel),
            },
            content_started: false,
            finished: false,
            ping_every,
        },
    };
    Sse::new(relay_stream(relay))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use uuid::Uuid;

    fn started() -> StreamEvent {
        StreamEvent::StreamStarted {
            request_id: Uuid::nil(),
            message_id: Uuid::nil(),
            is_new_turn: true,
            thread_summary_applied: None,
        }
    }

    async fn collect(relay: Relay) -> Vec<String> {
        let mut r = relay;
        let mut out = Vec::new();
        while let Some(item) = r.next().await {
            out.push(match item {
                Item::Ping => "ping".to_owned(),
                Item::Ev(ev) => sse_parts(ev).0.to_owned(),
            });
        }
        out
    }

    #[tokio::test]
    async fn interrupted_when_channel_closes() {
        let (tx, rx) = mpsc::channel(4);
        tx.send(StreamEvent::Delta {
            kind: "text",
            content: "a".into(),
        })
        .await
        .unwrap();
        drop(tx);
        let token = CancellationToken::new();
        let relay = Relay {
            pending: VecDeque::from([started()]),
            rx: Some(rx),
            guard: CancelGuard {
                token: Some(token.clone()),
            },
            content_started: false,
            finished: false,
            ping_every: Duration::from_secs(5),
        };
        let names = collect(relay).await;
        assert_eq!(names, vec!["stream_started", "delta", "error"]);
        assert!(!token.is_cancelled(), "terminal event disarms the guard");
    }

    #[tokio::test]
    async fn pings_only_before_content_and_cancel_on_drop() {
        let (tx, rx) = mpsc::channel(4);
        let token = CancellationToken::new();
        let mut relay = Relay {
            pending: VecDeque::from([started()]),
            rx: Some(rx),
            guard: CancelGuard {
                token: Some(token.clone()),
            },
            content_started: false,
            finished: false,
            ping_every: Duration::from_millis(20),
        };
        assert!(matches!(relay.next().await, Some(Item::Ev(_))));
        assert!(matches!(relay.next().await, Some(Item::Ping)));
        tx.send(StreamEvent::Delta {
            kind: "text",
            content: "x".into(),
        })
        .await
        .unwrap();
        assert!(matches!(relay.next().await, Some(Item::Ev(_))));
        // After content no ping: next() waits for the channel.
        let r = tokio::time::timeout(Duration::from_millis(80), relay.next()).await;
        assert!(r.is_err());
        drop(relay);
        assert!(token.is_cancelled());
        let _ = futures::stream::empty::<()>().next().await;
    }
}
