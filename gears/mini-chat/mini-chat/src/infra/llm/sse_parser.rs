//! Incremental `text/event-stream` parser (no buffering beyond one event).

/// One parsed server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser: feed byte chunks, get complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk; returns the events completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(ev) = self.handle_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush a trailing event without a final blank line (end of stream).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buf.is_empty() {
            let rest: Vec<u8> = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest).into_owned();
            let line = line.trim_end_matches('\r').to_owned();
            if let Some(ev) = self.handle_line(&line) {
                return Some(ev);
            }
        }
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let ev = SseEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        if ev.data.is_empty() && ev.event.is_none() {
            return None;
        }
        Some(ev)
    }

    fn handle_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
#[path = "sse_parser_tests.rs"]
mod tests;
