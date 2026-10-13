//! Incremental SSE parser (`event:` / `data:` lines, blank-line separated).

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser fed with arbitrary byte chunks.
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

    /// Feeds a chunk; returns completed events.
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
            self.process_line(&line, &mut out);
        }
        out
    }

    /// Flushes a trailing event at end of stream.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest).trim_end_matches('\r').to_owned();
            self.process_line(&line, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseFrame>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
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
    }

    fn dispatch(&mut self, out: &mut Vec<SseFrame>) {
        if self.data.is_empty() && self.event.is_none() {
            return;
        }
        let frame = SseFrame {
            event: self.event.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        if frame.data.is_empty() && frame.event.is_none() {
            return;
        }
        out.push(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_and_crlf() {
        let mut p = SseParser::new();
        let mut frames = p.feed(b"event: response.output_text.delta\r\ndata: {\"del");
        assert!(frames.is_empty());
        frames.extend(p.feed(b"ta\":\"hi\"}\r\n\r\ndata: [DONE]\n\n: comment\n\n"));
        frames.extend(p.finish());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("response.output_text.delta"));
        assert_eq!(frames[0].data, "{\"delta\":\"hi\"}");
        assert_eq!(frames[1].data, "[DONE]");
    }

    #[test]
    fn flushes_trailing_event_without_blank_line() {
        let mut p = SseParser::new();
        let mut frames = p.feed(b"data: a\ndata: b");
        frames.extend(p.finish());
        assert_eq!(frames, vec![SseFrame { event: None, data: "a\nb".to_owned() }]);
    }
}
