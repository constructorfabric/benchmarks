//! Streaming splitter of `<think>…</think>` blocks into reasoning deltas
//! (vLLM Responses adapter).

use crate::infra::llm::types::ProviderEvent;

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

#[derive(Debug, Default)]
pub struct ThinkSplitter {
    in_think: bool,
    pending: String,
}

impl ThinkSplitter {
    fn emit(&self, out: &mut Vec<ProviderEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.in_think {
            out.push(ProviderEvent::ReasoningDelta(text.to_owned()));
        } else {
            out.push(ProviderEvent::TextDelta(text.to_owned()));
        }
    }

    /// Feed a text delta.
    pub fn push(&mut self, delta: &str) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        self.pending.push_str(delta);
        loop {
            let tag = if self.in_think { CLOSE } else { OPEN };
            if let Some(pos) = self.pending.find(tag) {
                let before = self.pending[..pos].to_owned();
                self.emit(&mut out, &before);
                self.pending = self.pending[pos + tag.len()..].to_owned();
                self.in_think = !self.in_think;
                continue;
            }
            // Keep a possible partial tag at the end.
            let keep = (1..tag.len())
                .rev()
                .find(|n| self.pending.ends_with(&tag[..*n]))
                .unwrap_or(0);
            let split = self.pending.len() - keep;
            let ready = self.pending[..split].to_owned();
            self.emit(&mut out, &ready);
            self.pending = self.pending[split..].to_owned();
            break;
        }
        out
    }

    /// Flush buffered text at the end of the stream.
    pub fn flush(&mut self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.pending);
        self.emit(&mut out, &rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&str]) -> (String, String) {
        let mut s = ThinkSplitter::default();
        let mut evs = Vec::new();
        for c in chunks {
            evs.extend(s.push(c));
        }
        evs.extend(s.flush());
        let mut text = String::new();
        let mut reasoning = String::new();
        for e in evs {
            match e {
                ProviderEvent::TextDelta(t) => text.push_str(&t),
                ProviderEvent::ReasoningDelta(t) => reasoning.push_str(&t),
                _ => {}
            }
        }
        (text, reasoning)
    }

    #[test]
    fn splits_think_blocks_across_chunks() {
        let (t, r) = collect(&["<thi", "nk>plan</th", "ink>answer"]);
        assert_eq!(t, "answer");
        assert_eq!(r, "plan");
    }

    #[test]
    fn plain_text_passes_through() {
        let (t, r) = collect(&["hello ", "world <"]);
        assert_eq!(t, "hello world <");
        assert!(r.is_empty());
    }
}
