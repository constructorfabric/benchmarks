//! Minimal incremental SSE parser for provider streams.

/// One parsed server event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser: feed bytes, take complete frames.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
    pending: Vec<u8>,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns every frame completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.pending.extend_from_slice(chunk);
        // Keep incomplete UTF-8 sequences for the next chunk.
        let valid_up_to = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let text: Vec<u8> = self.pending.drain(..valid_up_to).collect();
        self.buf.push_str(&String::from_utf8_lossy(&text));
        if self.buf.contains('\r') {
            self.buf = self.buf.replace("\r\n", "\n").replace('\r', "\n");
        }
        let mut frames = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..pos + 2).collect();
            if let Some(f) = parse_block(&block) {
                frames.push(f);
            }
        }
        frames
    }

    /// Flushes a trailing frame without the final blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        let rest = std::mem::take(&mut self.buf);
        parse_block(&rest)
    }
}

fn parse_block(block: &str) -> Option<SseFrame> {
    let mut frame = SseFrame::default();
    let mut has_data = false;
    for line in block.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..])),
            None => (line, ""),
        };
        match field {
            "event" => frame.event = Some(value.to_owned()),
            "data" => {
                if has_data {
                    frame.data.push('\n');
                }
                frame.data.push_str(value);
                has_data = true;
            }
            _ => {}
        }
    }
    (has_data || frame.event.is_some()).then_some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_frames() {
        let mut p = SseParser::new();
        assert!(p.feed(b"event: a\ndata: {\"x\"").is_empty());
        let f = p.feed(b":1}\n\nevent: b\r\ndata: 2\r\n\r\n");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].event.as_deref(), Some("a"));
        assert_eq!(f[0].data, "{\"x\":1}");
        assert_eq!(f[1].data, "2");
    }

    #[test]
    fn multiline_data_and_comments() {
        let mut p = SseParser::new();
        let f = p.feed(b": keepalive\n\ndata: a\ndata: b\n\n");
        assert_eq!(f, vec![SseFrame { event: None, data: "a\nb".into() }]);
    }
}
