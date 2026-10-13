//! Incremental SSE parser over a byte stream.

/// One parsed SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// `event:` name.
    pub event: Option<String>,
    /// Joined `data:` lines.
    pub data: String,
}

/// Incremental parser (handles `\n` and `\r\n`, multi-line data, comments).
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds bytes and returns the completed frames.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let mut line: String = self.buf.drain(..=pos).collect();
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                if let Some(frame) = self.take_frame() {
                    out.push(frame);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f.to_owned(), v.strip_prefix(' ').unwrap_or(v).to_owned()),
                None => (line.clone(), String::new()),
            };
            match field.as_str() {
                "event" => self.event = Some(value),
                "data" => self.data.push(value),
                _ => {}
            }
        }
        out
    }

    /// Flushes a trailing frame without the final blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let mut frames = self.feed(format!("{rest}\n").as_bytes());
            if let Some(f) = frames.pop() {
                return Some(f);
            }
        }
        self.take_frame()
    }

    fn take_frame(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let frame = SseFrame { event: self.event.take(), data: self.data.join("\n") };
        self.data.clear();
        Some(frame)
    }
}
