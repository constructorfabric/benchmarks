//! Incremental SSE parser for provider streams (events are produced as soon
//! as their terminating blank line arrives; nothing is buffered beyond the
//! current event).

/// One parsed SSE event (`id` / `retry` are ignored).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` line, if any.
    pub event: Option<String>,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

impl SseEvent {
    /// Event name: the `event:` line, or — when it is missing or `message` —
    /// the `type` field of the JSON data.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        match self.event.as_deref() {
            Some(name) if !name.is_empty() && name != "message" => Some(name.to_owned()),
            _ => serde_json::from_str::<serde_json::Value>(&self.data)
                .ok()?
                .get("type")?
                .as_str()
                .map(str::to_owned),
        }
    }
}

/// Byte-level incremental parser.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current incomplete line.
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feed a chunk; returns the events completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                out.extend(self.dispatch());
            } else {
                self.field(&line);
            }
        }
        out
    }

    /// End of stream: the pending event, if it has data.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            self.field(line.trim_end_matches('\r'));
        }
        self.dispatch()
    }

    fn field(&mut self, line: &str) {
        if line.starts_with(':') {
            return;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match name {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
#[path = "sse_parser_tests.rs"]
mod tests;
