//! Incremental Server-Sent Events parser for provider streams.

/// One parsed SSE frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Byte-oriented incremental parser: feed chunks, collect complete frames.
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

    /// Feeds bytes and returns the frames completed by them.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(frame) = self.line(&line) {
                out.push(frame);
            }
        }
        out
    }

    /// Flushes a trailing frame without a final blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            let line = line.trim_end_matches('\r').to_owned();
            if let Some(f) = self.line(&line) {
                return Some(f);
            }
        }
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let frame = SseFrame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        };
        Some(frame)
    }

    fn line(&mut self, line: &str) -> Option<SseFrame> {
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
mod tests {
    use super::SseParser;

    #[test]
    fn parses_split_frames() {
        let mut p = SseParser::new();
        assert!(p.feed(b"event: a\ndata: {\"x\"").is_empty());
        let frames = p.feed(b":1}\n\ndata: b\r\n\r\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[0].data, "{\"x\":1}");
        assert_eq!(frames[1].event, None);
        assert_eq!(frames[1].data, "b");
    }

    #[test]
    fn comments_ignored_and_finish_flushes() {
        let mut p = SseParser::new();
        assert!(p.feed(b": keepalive\n\n").is_empty());
        assert!(p.feed(b"data: tail").is_empty());
        assert_eq!(p.finish().map(|f| f.data), Some("tail".to_owned()));
    }
}
