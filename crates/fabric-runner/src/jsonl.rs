//! Streaming JSONL decoding: one bounded line/event, never the whole output.
use std::io::{self, BufRead};

use crate::EventFormat;
pub use fabric_domain::EventCounts as Summary;
use serde_json::Value;

/// Fixed structured-record ceiling, excluding LF and including a possible CR.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
const INITIAL_LINE_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub enum Event {
    Parsed(Value),
    Text(String),
    Invalid,
    Oversized,
}

/// Oversized lines are drained through the next newline before decoding resumes.
/// serde_json's default recursion limit also bounds deeply nested event parsing.
pub struct Events<R> {
    reader: R,
    line: Vec<u8>,
    line_number: u64,
    line_bytes: u64,
    format: EventFormat,
}

impl<R: BufRead> Events<R> {
    pub fn new(reader: R) -> Self {
        Self::with_format(reader, EventFormat::JsonLines)
    }

    pub fn with_format(reader: R, format: EventFormat) -> Self {
        Self {
            reader,
            line: Vec::with_capacity(INITIAL_LINE_BYTES),
            line_number: 0,
            line_bytes: 0,
            format,
        }
    }

    pub fn line_number(&self) -> u64 {
        self.line_number
    }

    /// Raw line bytes, excluding LF but including a possible CR.
    pub fn line_bytes(&self) -> u64 {
        self.line_bytes
    }

    pub fn summarize(self) -> io::Result<Summary> {
        let mut summary = Summary::default();
        for event in self {
            match event? {
                Event::Parsed(_) => summary.parsed += 1,
                Event::Text(_) => summary.parsed += 1,
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
        self.line_bytes = 0;
        let mut oversized = false;
        let max_line_bytes = match self.format {
            EventFormat::JsonLines => MAX_LINE_BYTES,
            EventFormat::TextLines => 64 * 1024,
        };
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
            self.line_bytes += length as u64;
            if !oversized {
                if length > max_line_bytes - self.line.len() {
                    oversized = true;
                    self.line.clear();
                } else {
                    let needed = self.line.len() + length;
                    if needed > self.line.capacity() {
                        // Explicit power-of-two growth cannot overshoot the fixed ceiling.
                        self.line
                            .reserve_exact(needed.next_power_of_two() - self.line.len());
                    }
                    self.line.extend_from_slice(&chunk[..length]);
                }
            }
            self.reader.consume(length + usize::from(newline.is_some()));
            if newline.is_some() {
                break;
            }
        }
        self.line_number += 1;
        Some(Ok(if oversized {
            Event::Oversized
        } else {
            match self.format {
                EventFormat::JsonLines => match serde_json::from_slice(&self.line) {
                    Ok(value) => Event::Parsed(value),
                    Err(_) => Event::Invalid,
                },
                EventFormat::TextLines => match std::str::from_utf8(&self.line) {
                    Ok(text) => Event::Text(text.trim_end_matches('\r').to_owned()),
                    Err(_) => Event::Invalid,
                },
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
    fn small_streams_keep_a_small_reusable_buffer() {
        let mut events = Events::new(Cursor::new(b"{}\nnull\n"));
        assert_eq!(events.line.capacity(), INITIAL_LINE_BYTES);
        assert!(matches!(events.next(), Some(Ok(Event::Parsed(_)))));
        assert!(matches!(events.next(), Some(Ok(Event::Parsed(_)))));
        assert!(events.next().is_none());
        assert_eq!(events.line.capacity(), INITIAL_LINE_BYTES);
    }

    #[test]
    fn crlf_counts_cr_at_the_exact_boundary_and_recovers_after_one_byte_over() {
        let exact = format!("\"{}\"\r", "x".repeat(MAX_LINE_BYTES - 3));
        let bytes = format!("{exact}\n{exact} \nfalse");
        let mut events = Events::new(BufReader::with_capacity(257, Cursor::new(bytes)));
        assert!(matches!(events.next(), Some(Ok(Event::Parsed(_)))));
        assert_eq!(events.line_number(), 1);
        assert_eq!(events.line_bytes(), MAX_LINE_BYTES as u64);
        assert!(matches!(events.next(), Some(Ok(Event::Oversized))));
        assert_eq!(events.line_number(), 2);
        assert_eq!(events.line_bytes(), (MAX_LINE_BYTES + 1) as u64);
        assert!(matches!(
            events.next(),
            Some(Ok(Event::Parsed(Value::Bool(false))))
        ));
        assert_eq!(events.line_bytes(), 5);
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
