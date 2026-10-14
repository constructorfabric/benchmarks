//! The byte counter of the streamed provider upload
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload` steps
//! `inst-pfu-stream` and `inst-pfu-too-large`): counts every chunk, aborts above the effective
//! per-file limit, and keeps a copy of image bytes only while the count stays at or below the
//! thumbnail decode limit.

use toolkit_macros::domain_model;

/// The byte limit was exceeded.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitExceeded;

/// Counts the bytes of a body.
#[domain_model]
#[derive(Debug)]
pub struct BodyCounter {
    limit: u64,
    keep_up_to: Option<usize>,
    count: u64,
    kept: Option<Vec<u8>>,
}

impl BodyCounter {
    /// A counter that fails above `limit` bytes and keeps a copy of the body while the count is
    /// at most `keep_up_to` (`None`: never keep a copy).
    #[must_use]
    pub fn new(limit: u64, keep_up_to: Option<usize>) -> Self {
        Self {
            limit,
            keep_up_to,
            count: 0,
            kept: keep_up_to.map(|_| Vec::new()),
        }
    }

    /// Account one chunk.
    ///
    /// # Errors
    ///
    /// [`LimitExceeded`] once the total count is above the limit.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), LimitExceeded> {
        self.count = self.count.saturating_add(chunk.len() as u64);
        if self.count > self.limit {
            return Err(LimitExceeded);
        }
        if let Some(max) = self.keep_up_to {
            if usize::try_from(self.count).is_ok_and(|count| count <= max) {
                if let Some(kept) = self.kept.as_mut() {
                    kept.extend_from_slice(chunk);
                }
            } else {
                self.kept = None;
            }
        }
        Ok(())
    }

    /// The bytes counted so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The kept copy of the whole body: `Some` only when a copy was requested and the count
    /// never exceeded the keep limit.
    #[must_use]
    pub fn into_kept(self) -> Option<Vec<u8>> {
        self.kept
    }
}

#[cfg(test)]
#[path = "upload_body_tests.rs"]
mod upload_body_tests;
