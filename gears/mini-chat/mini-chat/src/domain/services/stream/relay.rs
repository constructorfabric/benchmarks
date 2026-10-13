//! The SSE relay between the provider task and the HTTP response (DESIGN
//! section 3.3, "SSE stream close rules"; section 5.7 "Terminal SSE Event
//! Emission Guard").

use futures::Stream;
use tokio::sync::mpsc;

use super::DisconnectGuard;
use super::events::{STREAM_INTERRUPTED, StreamEvent};

/// Message of the synthesized `stream_interrupted` error.
const INTERRUPTED_MESSAGE: &str =
    "The response stream was interrupted; check the turn status for the outcome";

/// The live event stream of a turn: every event of `rx` up to and including
/// the first terminal event, then the end of the stream. When the provider
/// task ends without a terminal event (CAS lost, panic), an
/// `error{stream_interrupted}` is appended. Dropping the returned stream (client
/// disconnect) drops `guard`, which cancels the provider task.
pub fn relay(
    rx: mpsc::Receiver<StreamEvent>,
    guard: DisconnectGuard,
) -> impl Stream<Item = StreamEvent> + Send + 'static {
    futures::stream::unfold(Some((rx, guard)), |state| async move {
        let (mut rx, guard) = state?;
        match rx.recv().await {
            Some(ev) if ev.is_terminal() => {
                drop(guard);
                Some((ev, None))
            }
            Some(ev) => Some((ev, Some((rx, guard)))),
            None => Some((
                StreamEvent::error(STREAM_INTERRUPTED, INTERRUPTED_MESSAGE),
                None,
            )),
        }
    })
}
