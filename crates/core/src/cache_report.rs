//! Whether the engine reports how many prompt tokens it read from its
//! prefix cache. Some engines (LM Studio 0.4) reuse the prefix but leave
//! `cached_tokens` out of the usage they return, and a harness that fills
//! the gap with 0 (Pi does) makes that look like no cache. The engine
//! proxy, which sees every response, notes each one; the preflight reads
//! the note.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// No response with usage yet.
const UNSEEN: u8 = 0;
/// Usage, without cached tokens.
const USAGE: u8 = 1;
/// Usage that named cached tokens.
const CACHED_TOKENS: u8 = 2;

/// What the engine's responses so far said about cached tokens.
#[derive(Clone, Debug, Default)]
pub struct CacheReport(Arc<AtomicU8>);

impl CacheReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes one response that carried usage, and whether it named its
    /// cached tokens (0 counts: the engine reported it).
    pub fn note(&self, named_cached_tokens: bool) {
        let seen = if named_cached_tokens {
            CACHED_TOKENS
        } else {
            USAGE
        };
        self.0.fetch_max(seen, Ordering::SeqCst);
    }

    /// `None` before any response with usage; then whether any of them
    /// named its cached tokens. An engine that names them only once they
    /// are above 0 still counts as reporting them.
    pub fn reported(&self) -> Option<bool> {
        match self.0.load(Ordering::SeqCst) {
            UNSEEN => None,
            seen => Some(seen == CACHED_TOKENS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_response_naming_cached_tokens_makes_the_engine_report_them() {
        let report = CacheReport::new();
        assert_eq!(report.reported(), None);
        let proxy_side = report.clone();
        proxy_side.note(false);
        assert_eq!(report.reported(), Some(false));
        proxy_side.note(true);
        proxy_side.note(false);
        assert_eq!(report.reported(), Some(true));
    }
}
