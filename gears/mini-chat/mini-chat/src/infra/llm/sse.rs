//! Incremental Server-Sent Events frame parser (WHATWG `text/event-stream`).
//!
//! Lines end with LF, CRLF or CR (a CRLF split across chunks counts once).
//! `event:` and `data:` fields are kept (several `data:` lines join with
//! `\n`), comment lines (`:`) and other fields (`id`, `retry`) are skipped. A
//! blank line dispatches the frame when it has data; a frame not terminated by
//! a blank line when the stream ends is dropped.

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field (`None` when the frame had none).
    pub event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

/// Feeds byte chunks in, yields complete frames.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current, not yet terminated line.
    line: Vec<u8>,
    /// The previous chunk ended with CR: skip a leading LF.
    pending_cr: bool,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Parse one chunk; returns the frames completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        let mut rest = chunk;
        if self.pending_cr {
            self.pending_cr = false;
            if let Some(stripped) = rest.strip_prefix(b"\n") {
                rest = stripped;
            }
        }
        while let Some(pos) = rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
            self.line.extend_from_slice(&rest[..pos]);
            let is_cr = rest[pos] == b'\r';
            rest = &rest[pos + 1..];
            if is_cr {
                match rest.first() {
                    Some(b'\n') => rest = &rest[1..],
                    None => self.pending_cr = true,
                    Some(_) => {}
                }
            }
            let line = std::mem::take(&mut self.line);
            if let Some(frame) = self.process_line(&line) {
                frames.push(frame);
            }
        }
        self.line.extend_from_slice(rest);
        frames
    }

    fn process_line(&mut self, line: &[u8]) -> Option<SseFrame> {
        if line.is_empty() {
            let event = self.event.take();
            if self.data.is_empty() {
                return None;
            }
            let data = std::mem::take(&mut self.data).join("\n");
            return Some(SseFrame { event, data });
        }
        if line.starts_with(b":") {
            return None;
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
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
#[path = "sse_tests.rs"]
mod sse_tests;
