//! A minimal Server-Sent Events decoder: bytes in, `(event, data)` out.
//! Handles chunk boundaries anywhere (mid-line, mid-UTF-8) and CRLF.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field, empty when absent.
    pub event: String,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl Decoder {
    /// Feeds a chunk; returns every event it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if let Some(ev) = self.dispatch() {
                    out.push(ev);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line.as_ref(), ""),
            };
            match field {
                "event" => self.event = value.to_owned(),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        out
    }

    /// Flushes a final event not followed by a blank line.
    pub fn finish(&mut self) -> Option<Event> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let mut out = self.push(&rest);
            out.extend(self.push(b"\n\n"));
            return out.pop();
        }
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<Event> {
        if self.data.is_empty() {
            self.event.clear();
            return None;
        }
        Some(Event { event: std::mem::take(&mut self.event), data: std::mem::take(&mut self.data).join("\n") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_split_chunks_and_crlf() {
        let raw = "event: message_start\r\ndata: {\"a\":1}\r\n\r\n: ping\n\ndata: one\ndata: two\n\n";
        let mut d = Decoder::default();
        let mut events = Vec::new();
        for b in raw.as_bytes().chunks(3) {
            events.extend(d.push(b));
        }
        assert_eq!(
            events,
            [
                Event { event: "message_start".into(), data: "{\"a\":1}".into() },
                Event { event: String::new(), data: "one\ntwo".into() },
            ]
        );
    }

    #[test]
    fn keeps_multibyte_across_chunks() {
        let raw = "data: café ☕\n\n".as_bytes();
        let mut d = Decoder::default();
        let mut events = d.push(&raw[..9]);
        events.extend(d.push(&raw[9..]));
        assert_eq!(events[0].data, "café ☕");
    }

    #[test]
    fn finish_flushes_unterminated_event() {
        let mut d = Decoder::default();
        assert!(d.push(b"data: [DONE]").is_empty());
        assert_eq!(d.finish().unwrap().data, "[DONE]");
        assert!(d.finish().is_none());
    }
}
