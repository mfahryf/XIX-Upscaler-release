//! Port of the `sseCollect` helper from `src/XIX-VectorizeAI_v2.js`.
//!
//! svg.new streams the vectorization result as Server-Sent Events. Blocks are
//! split on `\n\n`, each block's first `data: ` line holds a JSON payload, and
//! the LAST event whose `event` field matches the requested name wins. A
//! server `error` event aborts the collection.
//!
//! The HTTP status check lives in `net::http` (Task 6); this collector only
//! consumes the response body stream.

use std::fmt;

#[derive(Debug)]
pub enum SseError {
    Http(String),
    Server(String),
    NoEvent(String),
    Utf8,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SseError::Http(m) => write!(f, "HTTP error: {m}"),
            SseError::Server(m) => write!(f, "SSE error: {m}"),
            SseError::NoEvent(m) => write!(f, "no \"{m}\" event in stream"),
            SseError::Utf8 => write!(f, "stream is not valid utf-8"),
        }
    }
}

impl std::error::Error for SseError {}

pub struct SseCollector {
    event_name: String,
    buffer: String,
    collected: Option<String>,
    sse_error: Option<String>,
    /// Latest `progress` event percent (0..=100), if svg.new sent one. The
    /// caller polls this after each `push` to surface live progress.
    progress: Option<u8>,
}

impl SseCollector {
    pub fn new(event_name: &str) -> Self {
        SseCollector {
            event_name: event_name.to_string(),
            buffer: String::new(),
            collected: None,
            sse_error: None,
            progress: None,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        loop {
            let Some(idx) = self.buffer.find("\n\n") else {
                break;
            };
            let raw = self.buffer[..idx].to_string();
            self.buffer = self.buffer[idx + 2..].to_string();
            let line = raw.lines().find(|l| l.starts_with("data: "));
            let Some(line) = line else { continue };
            let Ok(evt) = serde_json::from_str::<serde_json::Value>(&line[6..]) else {
                continue; // malformed block — ignore
            };
            if evt.get("event").and_then(|e| e.as_str()) == Some(self.event_name.as_str()) {
                // JS truthiness: `evt.data || evt.payload?.svg || evt.payload?.image`
                let data = evt
                    .get("data")
                    .filter(|v| truthy(v))
                    .or_else(|| evt.pointer("/payload/svg").filter(|v| truthy(v)))
                    .or_else(|| evt.pointer("/payload/image").filter(|v| truthy(v)));
                if let Some(d) = data {
                    if d.is_string() {
                        self.collected = d.as_str().map(|s| s.to_string()); // last wins
                    }
                }
            }
            if evt.get("event").and_then(|e| e.as_str()) == Some("error") {
                self.sse_error = Some(evt.to_string()); // last error wins
            }
            if evt.get("event").and_then(|e| e.as_str()) == Some("progress") {
                // svg.new sends `{"event":"progress","percent":N,"stage":...}`.
                // Accept a number or a numeric string; clamp to 0..=100.
                let pct = evt.get("percent").and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                });
                if let Some(p) = pct {
                    self.progress = Some(p.round().clamp(0.0, 100.0) as u8);
                }
            }
        }
    }

    /// Return the latest progress percent seen since the previous call and
    /// clear it, so the caller only emits on change. `None` = no new progress.
    pub fn take_progress(&mut self) -> Option<u8> {
        self.progress.take()
    }

    pub fn finish(self) -> Result<String, SseError> {
        if let Some(e) = self.sse_error {
            return Err(SseError::Server(e.chars().take(200).collect()));
        }
        self.collected.ok_or(SseError::NoEvent(self.event_name))
    }
}

/// JS truthiness for JSON values (empty string / 0 / false / null are falsy).
fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_last_svg_event_across_chunks() {
        let mut c = SseCollector::new("svg");
        c.push(br#"data: {"event":"progress","data":"0"}"#);
        c.push(b"\n\n");
        c.push(br#"data: {"event":"svg","data":"<svg id='a'/>"}"#);
        c.push(b"\n\n");
        c.push(br#"data: {"event":"svg","payload":{"svg":"<svg id='b'/>"}}"#);
        c.push(b"\n\n");
        assert_eq!(c.finish().unwrap(), "<svg id='b'/>");
    }

    #[test]
    fn errors_when_server_reports_error_event() {
        let mut c = SseCollector::new("svg");
        c.push(br#"data: {"event":"error","payload":{"message":"boom"}}"#);
        c.push(b"\n\n");
        assert!(matches!(c.finish(), Err(SseError::Server(_))));
    }

    #[test]
    fn errors_when_no_matching_event() {
        let mut c = SseCollector::new("svg");
        c.push(br#"data: {"event":"progress","data":"42"}"#);
        c.push(b"\n\n");
        assert!(matches!(c.finish(), Err(SseError::NoEvent(_))));
    }

    #[test]
    fn ignores_malformed_blocks() {
        let mut c = SseCollector::new("svg");
        c.push(b"data: not-json\n\n");
        c.push(br#"data: {"event":"svg","data":"<svg/>"}"#);
        c.push(b"\n\n");
        assert_eq!(c.finish().unwrap(), "<svg/>");
    }

    #[test]
    fn captures_progress_percent_and_clears_on_take() {
        let mut c = SseCollector::new("svg");
        c.push(br#"data: {"event":"progress","percent":12,"stage":"trace"}"#);
        c.push(b"\n\n");
        assert_eq!(c.take_progress(), Some(12));
        assert_eq!(c.take_progress(), None, "take clears until next progress");
        c.push(br#"data: {"event":"progress","percent":"87"}"#);
        c.push(b"\n\n");
        assert_eq!(c.take_progress(), Some(87), "numeric string percent parsed");
    }
}
