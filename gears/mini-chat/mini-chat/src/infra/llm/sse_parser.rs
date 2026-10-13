//! Minimal incremental SSE frame parser.

/// One parsed SSE event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser: feed bytes, take complete frames.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a chunk and returns the frames it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        if self.buf.contains('\r') {
            self.buf = self.buf.replace("\r\n", "\n").replace('\r', "\n");
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let raw: String = self.buf.drain(..pos + 2).collect();
            if let Some(f) = parse_frame(&raw) {
                out.push(f);
            }
        }
        out
    }

    /// Flushes a trailing frame without the final blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        let raw = std::mem::take(&mut self.buf);
        parse_frame(&raw)
    }
}

fn parse_frame(raw: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (
                &line[..i],
                line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..]),
            ),
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
    Some(SseFrame {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_frames() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: a\ndata: {\"x\"").is_empty());
        let f = p.push(b":1}\n\ndata: b\r\n\r\n: comment\n\n");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].event.as_deref(), Some("a"));
        assert_eq!(f[0].data, "{\"x\":1}");
        assert_eq!(f[1].event, None);
        assert_eq!(f[1].data, "b");
        assert!(p.push(b"data: tail").is_empty());
        assert_eq!(p.finish().unwrap().data, "tail");
    }
}
