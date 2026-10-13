//! Incremental Server-Sent Events reader for provider streams: feeds raw body
//! chunks, yields complete events as soon as their terminating blank line
//! arrives (no buffering beyond one event).

/// One SSE event: the `event:` name (empty when absent) and the joined `data:` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Line-oriented SSE parser (LF, CRLF and CR line endings; chunk boundaries anywhere).
#[derive(Debug, Default)]
pub struct SseReader {
    /// Bytes of the current, not yet terminated line.
    buf: Vec<u8>,
    /// A line ended with `\r` at the end of the previous chunk: skip a leading `\n`.
    skip_lf: bool,
    /// The first line is still to come (UTF-8 BOM stripping).
    first_line: bool,
    event: Option<String>,
    data: Option<String>,
}

impl SseReader {
    #[must_use]
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    /// Feed a body chunk; returns the events completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        let mut out = Vec::new();
        let mut bytes = chunk;
        if self.skip_lf && !chunk.is_empty() {
            // The previous chunk ended with `\r`: a leading `\n` completes that CRLF.
            self.skip_lf = false;
            if let Some(rest) = bytes.strip_prefix(b"\n") {
                bytes = rest;
            }
        }
        while let Some(pos) = bytes.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.buf.extend_from_slice(&bytes[..pos]);
            let cr = bytes[pos] == b'\r';
            bytes = &bytes[pos + 1..];
            if cr {
                match bytes.first() {
                    Some(b'\n') => bytes = &bytes[1..],
                    None => self.skip_lf = true,
                    Some(_) => {}
                }
            }
            let line = std::mem::take(&mut self.buf);
            if let Some(frame) = self.line(&line) {
                out.push(frame);
            }
        }
        self.buf.extend_from_slice(bytes);
        out
    }

    /// End of body: returns the pending event, if any.
    pub fn finish(&mut self) -> Option<SseFrame> {
        let line = std::mem::take(&mut self.buf);
        if !line.is_empty() {
            // The last line has no terminator; it cannot complete an event by itself.
            let _ = self.line(&line);
        }
        self.dispatch()
    }

    /// Process one line; a blank line dispatches the pending event.
    fn line(&mut self, raw: &[u8]) -> Option<SseFrame> {
        let mut raw = raw;
        if self.first_line {
            self.first_line = false;
            raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
        }
        if raw.is_empty() {
            return self.dispatch();
        }
        let line = String::from_utf8_lossy(raw);
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
        None
    }

    /// Emit the pending event when it has data; reset the event state.
    fn dispatch(&mut self) -> Option<SseFrame> {
        let event = self.event.take().unwrap_or_default();
        self.data.take().map(|data| SseFrame { event, data })
    }
}

#[cfg(test)]
#[path = "sse_reader_tests.rs"]
mod sse_reader_tests;
