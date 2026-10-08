//! Minimal incremental SSE parser for provider byte streams.

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser: feed bytes, get complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
    pending_bytes: Vec<u8>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.pending_bytes.extend_from_slice(chunk);
        // Keep incomplete UTF-8 sequences for the next chunk.
        let valid = match std::str::from_utf8(&self.pending_bytes) {
            Ok(s) => s.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&self.pending_bytes[..valid]).into_owned();
        self.pending_bytes.drain(..valid);
        self.buf.push_str(&text.replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..pos + 2).collect();
            if let Some(ev) = parse_block(&block) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush a trailing event without the final blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let block = std::mem::take(&mut self.buf);
        parse_block(&block)
    }
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..])),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if event.is_none() && data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: a\nda").is_empty());
        let evs = p.feed(b"ta: {\"x\":1}\n\nevent: b\ndata: 2\n\n: comment\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("a"));
        assert_eq!(evs[0].data, "{\"x\":1}");
        assert_eq!(evs[1].data, "2");
    }

    #[test]
    fn handles_crlf_and_utf8_split() {
        let mut p = SseParser::default();
        let bytes = "data: h\u{e9}llo\r\n\r\n".as_bytes();
        let (a, b) = bytes.split_at(8);
        assert!(p.feed(a).is_empty());
        let evs = p.feed(b);
        assert_eq!(evs[0].data, "h\u{e9}llo");
    }
}
