//! The SSE body -> [`LlmEvent`] stream shared by every adapter: each adapter
//! supplies a [`Translate`] for its wire events.

use std::collections::VecDeque;

use futures::StreamExt;
use futures::stream::BoxStream;
use oagw_sdk::body::BodyStream;
use tokio_util::sync::CancellationToken;

use crate::infra::llm::sse::{SseFrame, SseParser};
use crate::infra::llm::types::{LlmEvent, ProviderError};

/// Per-stream translation of one adapter's SSE frames.
pub(super) trait Translate: Send {
    /// Events of one SSE frame.
    fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent>;

    /// Events when the body ends before a terminal event.
    fn on_end(&mut self) -> Vec<LlmEvent> {
        Vec::new()
    }
}

struct StreamState {
    body: BodyStream,
    parser: SseParser,
    translator: Box<dyn Translate>,
    queue: VecDeque<LlmEvent>,
    cancel: CancellationToken,
    done: bool,
}

/// Translate the provider SSE body into [`LlmEvent`]s. The stream ends after a
/// terminal event, at the end of the body, or when `cancel` fires (the body is
/// dropped, which aborts the upstream request).
pub(super) fn event_stream(
    body: BodyStream,
    translator: Box<dyn Translate>,
    cancel: CancellationToken,
) -> BoxStream<'static, LlmEvent> {
    let state = StreamState {
        body,
        parser: SseParser::default(),
        translator,
        queue: VecDeque::new(),
        cancel,
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        loop {
            if st.cancel.is_cancelled() {
                return None;
            }
            if let Some(ev) = st.queue.pop_front() {
                return Some((ev, st));
            }
            if st.done {
                return None;
            }
            let chunk = tokio::select! {
                biased;
                () = st.cancel.cancelled() => return None,
                chunk = st.body.next() => chunk,
            };
            match chunk {
                Some(Ok(bytes)) => {
                    for frame in st.parser.push(&bytes) {
                        let events = st.translator.on_frame(&frame);
                        st.push(events);
                        if st.done {
                            break;
                        }
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "provider stream read failed");
                    st.queue.push_back(LlmEvent::Failed {
                        error: ProviderError::provider("provider stream failed"),
                        usage: None,
                    });
                    st.done = true;
                }
                None => {
                    let tail = st.translator.on_end();
                    st.push(tail);
                    st.done = true;
                }
            }
        }
    }))
}

impl StreamState {
    /// Queue `events` up to and including the first terminal one.
    fn push(&mut self, events: Vec<LlmEvent>) {
        for ev in events {
            let terminal = is_terminal(&ev);
            self.queue.push_back(ev);
            if terminal {
                self.done = true;
                return;
            }
        }
    }
}

fn is_terminal(ev: &LlmEvent) -> bool {
    matches!(ev, LlmEvent::Completed { .. } | LlmEvent::Failed { .. })
}
