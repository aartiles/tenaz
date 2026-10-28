//! Splits an SSE body into events. Bytes are held until a blank line ends
//! an event and only then decoded, so a character the network splits
//! across two chunks arrives whole.

#[derive(Debug, Default)]
pub(crate) struct SseFramer {
    /// Bytes not yet terminated by a blank line.
    pending: Vec<u8>,
}

impl SseFramer {
    /// Adds `chunk` and returns the events it completes, without the blank
    /// line that ends each.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(at) = self.pending[start..].windows(2).position(|w| w == b"\n\n") {
            let end = start + at;
            events.push(String::from_utf8_lossy(&self.pending[start..end]).into_owned());
            start = end + 2;
        }
        self.pending.drain(..start);
        events
    }

    /// What is left after the last blank line, at the end of the body.
    pub(crate) fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_chunks_is_decoded_whole() {
        let event = "data: {\"content\":\"año 🦀\"}\n\n".as_bytes();
        let mut framer = SseFramer::default();
        let mut events = Vec::new();
        for byte in event {
            events.extend(framer.push(std::slice::from_ref(byte)));
        }
        assert_eq!(events, ["data: {\"content\":\"año 🦀\"}"]);
    }

    #[test]
    fn a_chunk_can_end_several_events_and_start_the_next() {
        let mut framer = SseFramer::default();
        assert_eq!(
            framer.push(b"data: 1\n\ndata: 2\n\ndata"),
            ["data: 1", "data: 2"]
        );
        assert_eq!(framer.push(b": 3\n\n"), ["data: 3"]);
    }

    #[test]
    fn finish_returns_the_unterminated_tail() {
        let mut framer = SseFramer::default();
        assert_eq!(framer.push(b"data: 1\n\ndata: 2\n"), ["data: 1"]);
        assert_eq!(framer.finish(), "data: 2\n");
    }
}
