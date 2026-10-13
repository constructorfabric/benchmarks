//! An incremental parser of `text/event-stream` bodies.
//!
//! The provider body arrives in arbitrary chunks (an event may be split
//! anywhere, also inside a multi-byte character); the parser buffers only the
//! incomplete tail and yields every complete event as soon as its terminating
//! blank line arrives.

/// One parsed server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field, when present.
    pub event: Option<String>,
    /// The `data:` lines joined by `\n`.
    pub data: String,
    /// The `id:` field, when present.
    pub id: Option<String>,
}

/// Incremental SSE parser.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current, unfinished line.
    line: Vec<u8>,
    /// The previous byte was a CR: a following LF belongs to the same break.
    skip_lf: bool,
    /// Fields of the event being assembled.
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    /// The first line of the body was processed (BOM handling).
    started: bool,
}

impl SseParser {
    /// A parser with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns the events it completed. Lines end with
    /// `\n`, `\r\n` or `\r`; comment lines (`:`) and unknown fields (`retry`)
    /// are ignored; a leading UTF-8 BOM is dropped; a block without `data`
    /// yields no event.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events),
                b'\r' => {
                    self.end_line(&mut events);
                    self.skip_lf = true;
                }
                other => self.line.push(other),
            }
        }
        events
    }

    /// Ends the body: a final event whose blank line never arrived but that
    /// has data is yielded (lenient to providers that omit the last blank
    /// line); an empty tail yields nothing.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events);
        }
        self.dispatch(&mut events);
        *self = Self::default();
        events
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) {
        let mut bytes = std::mem::take(&mut self.line);
        if !self.started {
            self.started = true;
            if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                bytes.drain(..3);
            }
        }
        if bytes.is_empty() {
            self.dispatch(events);
            return;
        }
        let line = String::from_utf8_lossy(&bytes);
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = self.event.take();
        let id = self.id.take();
        if let Some(data) = self.data.take() {
            events.push(SseEvent { event, data, id });
        }
    }
}

#[cfg(test)]
#[path = "sse_parse_tests.rs"]
mod tests;
