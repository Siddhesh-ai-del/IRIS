//! Minimal incremental SSE (text/event-stream) decoder (stage 1.3).
//!
//! Server-Sent Events arrive as an arbitrary byte stream: lines may be
//! split (or joined) at any position, line endings may be LF or CRLF, and
//! a UTF-8 character may straddle two TCP chunks. The decoder therefore
//! buffers **bytes**, only converting complete lines — a line terminated
//! at `\n` always contains whole UTF-8 characters, because no continuation
//! byte equals `0x0A`.
//!
//! We only dispatch `data:` payloads (OpenAI/OpenRouter streams never use
//! `event:` names); `event:`/`id:`/`retry:` fields and `:` comment lines
//! are accepted and ignored per the WHATWG SSE spec.

/// Incremental decoder: bytes in, complete `data:` payloads out.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// Unconsumed bytes (may end mid-line or mid-UTF-8 character).
    buffer: Vec<u8>,
    /// `data:` lines accumulated for the event currently being assembled.
    pending: Vec<String>,
}

impl SseDecoder {
    /// Feed a chunk of bytes; returns every `data:` payload completed by
    /// this chunk (a chunk can contain zero, one, or many events).
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop(); // the '\n' itself
            self.process_line(&line, &mut out);
        }
        out
    }

    /// End of stream: if the server stopped without a trailing blank line,
    /// dispatch the pending event anyway (lenient — some proxies strip the
    /// final newline). Returns `None` when nothing is pending.
    pub(crate) fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let mut out = Vec::new();
            self.process_line(&line, &mut out);
            if let Some(dispatched) = out.into_iter().next() {
                return Some(dispatched);
            }
        }
        if self.pending.is_empty() {
            None
        } else {
            let joined = self.pending.join("\n");
            self.pending.clear();
            Some(joined)
        }
    }

    /// Handle one complete line (without its `\n`/`\r\n` terminator).
    fn process_line(&mut self, line: &[u8], out: &mut Vec<String>) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            // Blank line: dispatch the event assembled so far.
            if !self.pending.is_empty() {
                out.push(self.pending.join("\n"));
                self.pending.clear();
            }
            return;
        }
        if line[0] == b':' {
            return; // comment (keep-alives)
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            // Field name without a colon = empty value (SSE quirk).
            None => (line, &[][..]),
        };
        if field == b"data" {
            // Exactly one leading space is stripped, if present.
            let value = value.strip_prefix(b" ").unwrap_or(value);
            // Safe: a line ends at 0x0A, which never occurs inside a
            // multi-byte UTF-8 sequence — only whole characters here.
            self.pending
                .push(String::from_utf8_lossy(value).into_owned());
        }
        // `event:`/`id:`/`retry:` fields are irrelevant to our data-only
        // streams — accepted and ignored.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(dec: &mut SseDecoder, chunks: &[&[u8]]) -> Vec<String> {
        chunks.iter().flat_map(|chunk| dec.push(chunk)).collect()
    }

    #[test]
    fn single_complete_event_dispatches_data_payload() {
        let mut dec = SseDecoder::default();
        let events = dec.push(b"data: {\"a\":1}\n\n");
        assert_eq!(events, vec!["{\"a\":1}"]);
    }

    #[test]
    fn events_split_across_arbitrary_chunks() {
        let mut dec = SseDecoder::default();
        // Split mid-prefix, mid-JSON, and between the two newlines.
        let events = push_all(
            &mut dec,
            &[
                b"da",
                b"ta: {\"te",
                b"xt\":\"hel",
                b"lo\"}\n\n",
                b"data: second\n",
                b"\n",
            ],
        );
        assert_eq!(events, vec!["{\"text\":\"hello\"}", "second"]);
    }

    #[test]
    fn crlf_line_endings_are_accepted() {
        let mut dec = SseDecoder::default();
        let events = dec.push(b"data: crlf\r\n\r\n");
        assert_eq!(events, vec!["crlf"]);
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut dec = SseDecoder::default();
        let events = dec.push(b"data: one\n\ndata: two\n\ndata: three\n\n");
        assert_eq!(events, vec!["one", "two", "three"]);
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let mut dec = SseDecoder::default();
        let events = dec.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(events, vec!["line1\nline2"]);
    }

    #[test]
    fn comments_and_other_fields_are_ignored() {
        let mut dec = SseDecoder::default();
        let events =
            dec.push(b": keep-alive\nevent: message\nid: 42\nretry: 100\ndata: payload\n\n");
        assert_eq!(events, vec!["payload"]);
    }

    #[test]
    fn utf8_character_split_across_chunks() {
        let mut dec = SseDecoder::default();
        let payload = "data: café\n\n".as_bytes();
        let (head, tail) = payload.split_at(10); // inside 'é' (0xC3 0xA9)
        let events = push_all(&mut dec, &[head, tail]);
        assert_eq!(events, vec!["café"]);
    }

    #[test]
    fn data_field_without_colon_yields_empty_line() {
        // SSE quirk: a field name with no colon means an empty value.
        let mut dec = SseDecoder::default();
        let events = dec.push(b"data\ndata: x\n\n");
        assert_eq!(events, vec!["\nx"]);
    }

    #[test]
    fn finish_flushes_event_without_trailing_blank_line() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b"data: tail").is_empty());
        assert_eq!(dec.finish(), Some("tail".to_string()));
        // Nothing pending afterwards.
        assert_eq!(dec.finish(), None);
    }

    #[test]
    fn finish_is_none_when_event_already_dispatched() {
        let mut dec = SseDecoder::default();
        dec.push(b"data: done\n\n");
        assert_eq!(dec.finish(), None);
    }
}
