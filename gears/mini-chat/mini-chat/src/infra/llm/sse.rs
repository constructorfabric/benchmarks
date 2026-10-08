//! Incremental `text/event-stream` decoder (no buffering beyond one event).

/// One decoded SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Feed bytes, pop complete events.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a chunk and return every event completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(frame) = self.handle_line(&line) {
                out.push(frame);
            }
        }
        out
    }

    /// Flush a trailing event without a final blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            for line in rest.lines() {
                if let Some(f) = self.handle_line(line) {
                    return Some(f);
                }
            }
        }
        self.dispatch()
    }

    fn handle_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let frame = SseFrame { event: self.event.take(), data: self.data.join("\n") };
        self.data.clear();
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_split_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"event: a\nda").is_empty());
        let f = d.push(b"ta: {\"x\":1}\n\n: comment\n\ndata: two\r\ndata: lines\r\n\r\n");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0], SseFrame { event: Some("a".into()), data: "{\"x\":1}".into() });
        assert_eq!(f[1], SseFrame { event: None, data: "two\nlines".into() });
    }

    #[test]
    fn finish_flushes_tail() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: tail").is_empty());
        assert_eq!(d.finish(), Some(SseFrame { event: None, data: "tail".into() }));
    }
}
