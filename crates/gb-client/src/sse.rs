//! Minimal incremental server-sent-events parser.
//!
//! Handles `event:`/`data:` fields, `:comment` keep-alives, both `\n` and
//! `\r\n` line endings, multi-line `data:`, and chunks split anywhere.

/// One complete SSE message (fields collected up to a blank line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub event: Option<String>,
    /// `data:` lines joined with `\n` (may be empty).
    pub data: String,
}

#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; returns every event completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<RawEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=end).collect();
            line.pop(); // \n
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(ev) = self.push_line(&String::from_utf8_lossy(&line)) {
                out.push(ev);
            }
        }
        out
    }

    fn push_line(&mut self, line: &str) -> Option<RawEvent> {
        if line.is_empty() {
            // Blank line dispatches the pending event, if any fields arrived.
            if self.event.is_none() && self.data.is_empty() {
                return None;
            }
            return Some(RawEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        }
        if line.starts_with(':') {
            return None; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_event() {
        let mut p = Parser::new();
        let evs = p.feed(b"event: status\ndata: {\"a\":1}\n\n");
        assert_eq!(
            evs,
            vec![RawEvent {
                event: Some("status".into()),
                data: "{\"a\":1}".into(),
            }]
        );
    }

    #[test]
    fn handles_split_chunks_and_crlf() {
        let mut p = Parser::new();
        assert!(p.feed(b"event: lo").is_empty());
        assert!(p.feed(b"g\r\nda").is_empty());
        let evs = p.feed(b"ta: hi\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("log"));
        assert_eq!(evs[0].data, "hi");
    }

    #[test]
    fn joins_multiline_data_and_skips_comments() {
        let mut p = Parser::new();
        let evs = p.feed(b": ping\n\ndata: one\ndata: two\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event, None);
        assert_eq!(evs[0].data, "one\ntwo");
    }

    #[test]
    fn emits_multiple_events_per_chunk() {
        let mut p = Parser::new();
        let evs = p.feed(b"data: 1\n\ndata: 2\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].data, "2");
    }

    #[test]
    fn ignores_blank_dispatch_without_fields() {
        let mut p = Parser::new();
        assert!(p.feed(b"\n\n").is_empty());
    }
}
