//! An incremental Server-Sent Events parser for the streamable HTTP
//! transport's response bodies.
//!
//! Hand-rolled for the same reason the stdio framing is: this is a
//! parser boundary fed by a remote peer, and the proxy owns its trust
//! boundaries. The parser is sans-I/O — bytes in, events out — with a
//! hard per-event size cap and typed handling of every failure, no
//! panics; it is a designated fuzz seam alongside `framing` (T5).
//!
//! Implements the WHATWG event-stream grammar as far as MCP needs it:
//! `data` accumulation across lines, comment lines, `retry` hints, CRLF
//! / LF / CR line endings split arbitrarily across chunks, and a leading
//! BOM. `id` fields are tolerated but ignored — stream resumability is
//! deliberately not implemented in T1/M2 — and events whose type is not
//! the default `message` are surfaced with their type so the caller can
//! ignore them knowingly.
//!
//! One deliberate divergence from the WHATWG decoder: invalid UTF-8 is
//! *not* repaired with replacement characters. Lossy repair inside a
//! JSON string would silently alter payload bytes the proxy promises to
//! forward untouched, so an event containing invalid UTF-8 is discarded
//! whole ([`SseItem::InvalidUtf8`]) and accounted, like an oversized
//! one — fail closed over fail garbled.

use std::time::Duration;

/// One parsed item from the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum SseItem {
    /// A dispatched event with non-empty data.
    Event {
        /// The event type, `None` for the default (`message`).
        event_type: Option<String>,
        /// The joined data payload (multi-line data joined with `\n`).
        data: String,
    },
    /// An event was discarded because it exceeded the size cap. The
    /// stream itself is fine and stays synchronized.
    Oversized,
    /// An event was discarded because a line of it was not valid
    /// UTF-8; repairing it would corrupt payload bytes.
    InvalidUtf8,
    /// The server sent a `retry` reconnection hint.
    Retry(Duration),
}

/// Incremental SSE parser with a per-event byte cap.
#[derive(Debug)]
pub struct SseParser {
    max_event_bytes: usize,
    /// The current, possibly incomplete line.
    line: Vec<u8>,
    /// Accumulated `data` lines of the current event.
    data: String,
    /// The current event's `event` field, if any.
    event_type: Option<String>,
    /// The current event blew the cap; discard it at dispatch.
    oversized: bool,
    /// The current event contained invalid UTF-8; discard it at
    /// dispatch.
    invalid_utf8: bool,
    /// The previous byte was a CR — an immediately following LF is part
    /// of the same line ending.
    swallow_lf: bool,
    /// Before the first line: strip a UTF-8 BOM if present.
    at_start: bool,
}

impl SseParser {
    /// A parser enforcing `max_event_bytes` over both any single line
    /// and an event's accumulated data.
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            line: Vec::new(),
            data: String::new(),
            event_type: None,
            oversized: false,
            invalid_utf8: false,
            swallow_lf: false,
            at_start: true,
        }
    }

    /// Feeds a chunk of bytes; returns every item completed by it.
    ///
    /// Chunks may split lines, line endings, and multi-byte UTF-8
    /// sequences at any byte; state carries across calls. An event is
    /// dispatched only when its terminating blank line arrives, so a
    /// chunk may complete zero, one, or several items. Events that blew
    /// the size cap or contained invalid UTF-8 are reported as
    /// [`SseItem::Oversized`]/[`SseItem::InvalidUtf8`] in place of the
    /// event; the stream stays synchronized. Call [`SseParser::finish`]
    /// when the body ends.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseItem> {
        let mut items = Vec::new();
        for &byte in chunk {
            match byte {
                b'\n' if self.swallow_lf => {
                    self.swallow_lf = false;
                }
                b'\n' => {
                    self.end_line(&mut items);
                }
                b'\r' => {
                    self.end_line(&mut items);
                    self.swallow_lf = true;
                }
                _ => {
                    self.swallow_lf = false;
                    if self.line.len() < self.max_event_bytes {
                        self.line.push(byte);
                    } else {
                        // The line alone blew the cap; the rest of it
                        // is discarded and the event it belongs to is
                        // poisoned.
                        self.oversized = true;
                    }
                }
            }
        }
        items
    }

    /// Signals end of stream. Per the SSE grammar an event not followed
    /// by a blank line is never dispatched, so this only reports
    /// whether data was discarded.
    pub fn finish(&mut self) -> bool {
        let incomplete =
            !self.data.is_empty() || !self.line.is_empty() || self.oversized || self.invalid_utf8;
        self.line.clear();
        self.reset_event();
        incomplete
    }

    fn end_line(&mut self, items: &mut Vec<SseItem>) {
        let raw = std::mem::take(&mut self.line);
        // Strict decoding: repairing damage would silently mutate
        // payload bytes, so an event with a bad line is poisoned and
        // discarded whole at dispatch.
        let Ok(decoded) = std::str::from_utf8(&raw) else {
            self.invalid_utf8 = true;
            self.at_start = false;
            return;
        };
        let mut line: &str = decoded;
        if self.at_start {
            self.at_start = false;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }

        if line.is_empty() {
            self.dispatch(items);
            return;
        }
        if line.starts_with(':') {
            return;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, rest)) => (field, rest.strip_prefix(' ').unwrap_or(rest)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.data.len() + value.len() + 1 > self.max_event_bytes {
                    self.oversized = true;
                    self.data.clear();
                } else if !self.oversized {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
            }
            "event" => self.event_type = Some(value.to_owned()),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(ms) = value.parse::<u64>() {
                    items.push(SseItem::Retry(Duration::from_millis(ms)));
                }
            }
            // `id` (resumability) and unknown fields are ignored.
            _ => {}
        }
    }

    fn dispatch(&mut self, items: &mut Vec<SseItem>) {
        if self.invalid_utf8 {
            items.push(SseItem::InvalidUtf8);
        } else if self.oversized {
            items.push(SseItem::Oversized);
        } else if !self.data.is_empty() {
            let mut data = std::mem::take(&mut self.data);
            if data.ends_with('\n') {
                data.pop();
            }
            items.push(SseItem::Event {
                event_type: self.event_type.take(),
                data,
            });
        }
        // An empty-data event (e.g. the priming event MCP servers send
        // with only an id) dispatches nothing but still resets state.
        self.reset_event();
    }

    fn reset_event(&mut self) {
        self.data.clear();
        self.event_type = None;
        self.oversized = false;
        self.invalid_utf8 = false;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn collect(parser: &mut SseParser, input: &[u8]) -> Vec<SseItem> {
        parser.push(input)
    }

    fn event(data: &str) -> SseItem {
        SseItem::Event {
            event_type: None,
            data: data.to_owned(),
        }
    }

    #[test]
    fn parses_single_event() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"data: {\"jsonrpc\":\"2.0\"}\n\n");
        assert_eq!(items, vec![event("{\"jsonrpc\":\"2.0\"}")]);
    }

    #[test]
    fn joins_multiline_data_with_newlines() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"data: {\ndata: \"a\": 1}\n\n");
        assert_eq!(items, vec![event("{\n\"a\": 1}")]);
    }

    #[test]
    fn handles_all_line_endings_split_across_chunks() {
        let mut p = SseParser::new(1024);
        let mut items = Vec::new();
        // CRLF split across a chunk boundary must not double-terminate.
        items.extend(p.push(b"data: one\r"));
        items.extend(p.push(b"\n\r\ndata: two\r\rdata: three\n\n"));
        assert_eq!(items, vec![event("one"), event("two"), event("three")]);
    }

    #[test]
    fn ignores_comments_ids_and_unknown_fields() {
        let mut p = SseParser::new(1024);
        let items = collect(
            &mut p,
            b": keepalive\nid: 42\nunknown: x\nunknown-no-colon\ndata: payload\n\n",
        );
        assert_eq!(items, vec![event("payload")]);
    }

    #[test]
    fn empty_data_priming_event_dispatches_nothing() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"id: prime-1\n\ndata: real\n\n");
        assert_eq!(items, vec![event("real")]);
    }

    #[test]
    fn event_type_is_surfaced() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"event: endpoint\ndata: /old-transport\n\n");
        assert_eq!(
            items,
            vec![SseItem::Event {
                event_type: Some("endpoint".to_owned()),
                data: "/old-transport".to_owned(),
            }]
        );
        // The type buffer resets between events.
        let items = collect(&mut p, b"data: x\n\n");
        assert_eq!(items, vec![event("x")]);
    }

    #[test]
    fn retry_hints_are_reported_and_bad_ones_ignored() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"retry: 1500\nretry: 12a\nretry:\n\n");
        assert_eq!(items, vec![SseItem::Retry(Duration::from_millis(1500))]);
    }

    #[test]
    fn value_space_stripping_is_single_and_optional() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"data:no-space\n\ndata:  two-spaces\n\n");
        assert_eq!(items, vec![event("no-space"), event(" two-spaces")]);
    }

    #[test]
    fn oversized_event_is_reported_and_stream_resyncs() {
        let mut p = SseParser::new(32);
        let mut input = b"data: ".to_vec();
        input.extend(vec![b'x'; 100]);
        input.extend_from_slice(b"\n\ndata: ok\n\n");
        let items = p.push(&input);
        assert_eq!(items, vec![SseItem::Oversized, event("ok")]);
    }

    #[test]
    fn oversized_accumulated_data_is_reported() {
        let mut p = SseParser::new(32);
        // Each line is under the cap; together they are not.
        let items = collect(
            &mut p,
            b"data: aaaaaaaaaaaaaaa\ndata: bbbbbbbbbbbbbbb\ndata: c\n\ndata: ok\n\n",
        );
        assert_eq!(items, vec![SseItem::Oversized, event("ok")]);
    }

    #[test]
    fn strips_leading_bom_only_at_stream_start() {
        let mut p = SseParser::new(1024);
        let items = collect(&mut p, b"\xef\xbb\xbfdata: first\n\n");
        assert_eq!(items, vec![event("first")]);
    }

    #[test]
    fn finish_reports_incomplete_trailing_event() {
        let mut p = SseParser::new(1024);
        assert!(collect(&mut p, b"data: never-terminated\n").is_empty());
        assert!(p.finish());
        let mut p = SseParser::new(1024);
        assert!(collect(&mut p, b"data: done\n\n").len() == 1);
        assert!(!p.finish());
    }

    #[test]
    fn invalid_utf8_discards_the_event_and_stream_resyncs() {
        let mut p = SseParser::new(1024);
        // The whole event is poisoned — including data lines that were
        // themselves fine — and the next event is unaffected.
        let items = collect(&mut p, b"data: ok-line\ndata: \xff\xfe\n\ndata: after\n\n");
        assert_eq!(items, vec![SseItem::InvalidUtf8, event("after")]);
    }
}
