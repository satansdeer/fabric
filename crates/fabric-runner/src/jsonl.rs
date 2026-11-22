//! Streaming JSONL decoding: one bounded line/event, never the whole output.
use std::io::{self, BufRead};

use serde::Serialize;
use serde_json::Value;

pub const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum Event {
    Parsed(Value),
    Invalid,
    Oversized,
}

#[derive(Default, Serialize)]
pub struct Summary {
    pub parsed: u64,
    pub invalid: u64,
    pub oversized: u64,
}

/// Oversized lines are drained through the next newline before decoding resumes.
/// serde_json's default recursion limit also bounds deeply nested event parsing.
pub struct Events<R> {
    reader: R,
    line: Vec<u8>,
}

impl<R: BufRead> Events<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::with_capacity(MAX_LINE_BYTES),
        }
    }

    pub fn summarize(self) -> io::Result<Summary> {
        let mut summary = Summary::default();
        for event in self {
            match event? {
                Event::Parsed(_) => summary.parsed += 1,
                Event::Invalid => summary.invalid += 1,
                Event::Oversized => summary.oversized += 1,
            }
        }
        Ok(summary)
    }
}

impl<R: BufRead> Iterator for Events<R> {
    type Item = io::Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        self.line.clear();
        let mut oversized = false;
        loop {
            let chunk = match self.reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(error) => return Some(Err(error)),
            };
            if chunk.is_empty() {
                if self.line.is_empty() && !oversized {
                    return None;
                }
                break;
            }
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let length = newline.unwrap_or(chunk.len());
            if !oversized {
                if length > MAX_LINE_BYTES - self.line.len() {
                    oversized = true;
                    self.line.clear();
                } else {
                    self.line.extend_from_slice(&chunk[..length]);
                }
            }
            self.reader.consume(length + usize::from(newline.is_some()));
            if newline.is_some() {
                break;
            }
        }
        Some(Ok(if oversized {
            Event::Oversized
        } else {
            match serde_json::from_slice(&self.line) {
                Ok(value) => Event::Parsed(value),
                Err(_) => Event::Invalid,
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    #[test]
    fn bounded_lines_recover_preserve_values_and_never_grow_with_output() {
        let exact = format!("\"{}\"", "x".repeat(MAX_LINE_BYTES - 2));
        let prefix = Cursor::new(format!("{exact}\n"));
        let huge = io::repeat(b'x').take(100 * 1024 * 1024);
        let suffix = Cursor::new(b"\n{\"value\":42}\r\nnull\n\ninvalid\n{\"final\":true}");
        let mut events = Events::new(BufReader::with_capacity(
            257,
            prefix.chain(huge).chain(suffix),
        ));
        let Some(Ok(Event::Parsed(Value::String(value)))) = events.next() else {
            panic!("exactly capped JSON string should parse");
        };
        assert_eq!(value.len(), MAX_LINE_BYTES - 2);
        assert!(matches!(events.next(), Some(Ok(Event::Oversized))));
        let Some(Ok(Event::Parsed(value))) = events.next() else {
            panic!("must resume after oversized line");
        };
        assert_eq!(value["value"], 42);
        assert!(matches!(
            events.next(),
            Some(Ok(Event::Parsed(Value::Null)))
        ));
        assert!(matches!(events.next(), Some(Ok(Event::Invalid))));
        assert!(matches!(events.next(), Some(Ok(Event::Invalid))));
        let Some(Ok(Event::Parsed(value))) = events.next() else {
            panic!("must parse final unterminated line");
        };
        assert_eq!(value["final"], true);
        assert!(events.next().is_none());
        assert_eq!(events.line.capacity(), MAX_LINE_BYTES);
    }

    #[test]
    fn oversize_unterminated_invalid_utf8_and_deep_json_are_bounded() {
        let deep = format!("{}0{}\n", "[".repeat(1000), "]".repeat(1000));
        let bytes = Cursor::new(deep.into_bytes())
            .chain(Cursor::new(b"\xff\n"))
            .chain(io::repeat(b'x').take((MAX_LINE_BYTES + 1) as u64));
        let stats = Events::new(BufReader::new(bytes)).summarize().unwrap();
        assert_eq!((stats.parsed, stats.invalid, stats.oversized), (0, 2, 1));
        assert_eq!(Events::new(Cursor::new(b"")).count(), 0);
    }
}
