//! Splits an SSE body into events and reads the JSON of their `data:`
//! lines.

/// Splits an SSE body into events. Bytes are held until a blank line ends
/// an event and only then decoded, so a character the network splits
/// across two chunks arrives whole. Lines may end with `\n` or `\r\n`,
/// mixed in one stream; a bare `\r` does not end a line.
#[derive(Debug, Default)]
pub(crate) struct SseFramer {
    /// Bytes not yet terminated by a blank line.
    pending: Vec<u8>,
}

impl SseFramer {
    /// Adds `chunk` and returns the events it completes, each without its
    /// last line ending and the blank line after it.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some((end, next)) = blank_line(&self.pending, start) {
            events.push(String::from_utf8_lossy(&self.pending[start..end]).into_owned());
            start = next;
        }
        self.pending.drain(..start);
        events
    }

    /// The last event, which the end of the body ends, read as if a blank
    /// line followed it; `None` when it is blank. `pending` holds no blank
    /// line, so this gives at most one event.
    pub(crate) fn finish(&mut self) -> Option<String> {
        let last = self.push(b"\n\n").into_iter().next();
        self.pending.clear();
        last.filter(|event| !event.trim().is_empty())
    }
}

/// The first blank line in `bytes` from `from` on: where the event before
/// it ends, without its last line ending, and where the next one starts.
fn blank_line(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut at = from;
    loop {
        at += bytes[at..].iter().position(|&b| b == b'\n')?;
        let blank = match &bytes[at + 1..] {
            [b'\n', ..] => 1,
            [b'\r', b'\n', ..] => 2,
            _ => {
                at += 1;
                continue;
            }
        };
        let end = if bytes[from..at].ends_with(b"\r") {
            at - 1
        } else {
            at
        };
        return Some((end, at + 1 + blank));
    }
}

/// The JSON of each `data:` line of an SSE event or body. Other lines, and
/// data that is not JSON such as `[DONE]`, are skipped.
pub(crate) fn data_json(text: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_chunks_is_decoded_whole() {
        for ending in ["\n\n", "\r\n\r\n", "\n\r\n"] {
            let event = format!("data: {{\"content\":\"año 🦀\"}}{ending}");
            let mut framer = SseFramer::default();
            let mut events = Vec::new();
            for byte in event.as_bytes() {
                events.extend(framer.push(std::slice::from_ref(byte)));
            }
            assert_eq!(events, ["data: {\"content\":\"año 🦀\"}"], "{ending:?}");
        }
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
    fn a_blank_line_written_with_crlf_ends_an_event() {
        let mut framer = SseFramer::default();
        assert_eq!(framer.push(b"data: 1\r\n\r\ndata: 2\r\n\r"), ["data: 1"]);
        assert_eq!(framer.push(b"\n"), ["data: 2"]);
    }

    #[test]
    fn the_lines_inside_an_event_keep_their_endings() {
        let mut framer = SseFramer::default();
        assert_eq!(
            framer.push(b"event: x\r\ndata: 1\r\n\r\n"),
            ["event: x\r\ndata: 1"]
        );
    }

    #[test]
    fn a_blank_line_with_nothing_before_it_is_an_empty_event() {
        let mut framer = SseFramer::default();
        assert_eq!(framer.push(b"\n\n\r\n\r\ndata: 1\n\n"), ["", "", "data: 1"]);
    }

    /// Unlike the SSE standard, which also ends a line at a bare `\r`.
    #[test]
    fn a_bare_cr_does_not_end_a_line() {
        let mut framer = SseFramer::default();
        assert_eq!(
            framer.push(b"data: 1\r\rdata: 2\n\n"),
            ["data: 1\r\rdata: 2"]
        );
    }

    #[test]
    fn a_blank_line_can_mix_crlf_and_lf() {
        let mut framer = SseFramer::default();
        assert_eq!(
            framer.push(b"data: 1\r\n\ndata: 2\n\r\ndata: 3\n\n"),
            ["data: 1", "data: 2", "data: 3"]
        );
    }

    #[test]
    fn only_the_json_of_data_lines_is_read() {
        let event = "event: x\r\n: ping\r\ndata:{\"a\":1}\r\ndata: [DONE]\r\ndata: {\"b\":2}\r\n";
        assert_eq!(
            data_json(event).collect::<Vec<_>>(),
            [serde_json::json!({"a": 1}), serde_json::json!({"b": 2})]
        );
    }

    /// What `finish` returns for `tail` after a whole event.
    fn finished(tail: &str) -> Option<String> {
        let mut framer = SseFramer::default();
        assert_eq!(
            framer.push(format!("data: 1\n\n{tail}").as_bytes()),
            ["data: 1"]
        );
        framer.finish()
    }

    #[test]
    fn finish_returns_the_unterminated_tail_without_its_line_endings() {
        for tail in [
            "data: 2",
            "data: 2\n",
            "data: 2\r\n",
            "data: 2\r",
            "data: 2\n\r",
            "data: 2\r\n\r",
        ] {
            assert_eq!(finished(tail).as_deref(), Some("data: 2"), "{tail:?}");
        }
    }

    #[test]
    fn finish_returns_nothing_for_a_blank_tail() {
        for tail in ["", "\r\n", "\n\r", " \n", "\t"] {
            assert_eq!(finished(tail), None, "{tail:?}");
        }
    }
}
