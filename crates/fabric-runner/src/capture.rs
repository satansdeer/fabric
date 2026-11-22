//! One shared byte budget for raw streams, structured JSONL and the marker.
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::sync::Mutex;

use fabric_domain::{CaptureMetadata, CapturedFile, EventCounts};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::jsonl::{Event, Events};

pub const TRUNCATION_MARKER: &[u8] = b"\n[FABRIC CAPTURE TRUNCATED]\n";

/// Files must be freshly created private outputs. `None` disables event decoding
/// for version probes; ordinary runs always supply an events.jsonl file.
pub struct Capture {
    state: Mutex<State>,
    decode_events: bool,
}

impl Capture {
    pub fn new(
        stdout: File,
        stderr: File,
        events: Option<File>,
        limit_bytes: u64,
    ) -> io::Result<Self> {
        if limit_bytes < TRUNCATION_MARKER.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Capture limit must fit the truncation marker.",
            ));
        }
        Ok(Self {
            decode_events: events.is_some(),
            state: Mutex::new(State {
                files: [
                    Sink::new(Some(stdout)),
                    Sink::new(Some(stderr)),
                    Sink::new(events),
                ],
                limit_bytes,
                persisted_bytes: 0,
                truncated: false,
                seen: [0, 0],
                error: None,
            }),
        })
    }

    pub(crate) fn stdout(&self, pipe: impl Read) -> io::Result<EventCounts> {
        let mut reader = RawReader {
            pipe,
            capture: self,
            stream: 0,
        };
        let mut counts = EventCounts::default();
        if !self.decode_events {
            io::copy(&mut reader, &mut io::sink())?;
            return Ok(counts);
        }
        let mut events = Events::new(BufReader::new(reader));
        // Reuse one bounded serialization buffer, never retain event values.
        let mut encoded = Vec::new();
        while let Some(event) = events.next() {
            let event = event?;
            let (status, value) = match &event {
                Event::Parsed(value) => {
                    counts.parsed += 1;
                    ("parsed", Some(value))
                }
                Event::Invalid => {
                    counts.invalid += 1;
                    ("invalid", None)
                }
                Event::Oversized => {
                    counts.oversized += 1;
                    ("oversized", None)
                }
            };
            let mut state = self.state.lock().unwrap();
            if state.truncated || state.error.is_some() {
                continue;
            }
            encoded.clear();
            serde_json::to_writer(
                &mut encoded,
                &Record {
                    line_number: events.line_number(),
                    line_bytes: events.line_bytes(),
                    status,
                    value,
                },
            )?;
            encoded.push(b'\n');
            state.persist(2, &encoded, true);
        }
        Ok(counts)
    }

    pub(crate) fn stderr(&self, pipe: impl Read) -> io::Result<()> {
        io::copy(
            &mut RawReader {
                pipe,
                capture: self,
                stream: 1,
            },
            &mut io::sink(),
        )?;
        Ok(())
    }

    pub(crate) fn finish(self, event_counts: EventCounts) -> io::Result<CaptureMetadata> {
        let mut state = self.state.into_inner().unwrap();
        if let Some(error) = state.error.take() {
            return Err(error);
        }
        if state.truncated {
            state.files[0].write(TRUNCATION_MARKER)?;
            state.persisted_bytes += TRUNCATION_MARKER.len() as u64;
        }
        for sink in &state.files {
            if let Some(file) = &sink.file {
                file.sync_all()?;
            }
        }
        let [stdout, stderr, events] = state.files.map(Sink::metadata);
        Ok(CaptureMetadata {
            limit_bytes: state.limit_bytes,
            persisted_bytes: state.persisted_bytes,
            truncated: state.truncated,
            stdout_bytes_seen: state.seen[0],
            stderr_bytes_seen: state.seen[1],
            stdout,
            stderr,
            events,
            event_counts,
        })
    }
}

#[derive(Serialize)]
struct Record<'a> {
    line_number: u64,
    line_bytes: u64,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a serde_json::Value>,
}

struct RawReader<'a, R> {
    pipe: R,
    capture: &'a Capture,
    stream: usize,
}
impl<R: Read> Read for RawReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = self.pipe.read(buffer)?;
        let mut state = self.capture.state.lock().unwrap();
        state.seen[self.stream] += length as u64;
        state.persist(self.stream, &buffer[..length], false);
        Ok(length)
    }
}

struct State {
    files: [Sink; 3],
    limit_bytes: u64,
    persisted_bytes: u64,
    truncated: bool,
    seen: [u64; 2],
    error: Option<io::Error>,
}
impl State {
    fn persist(&mut self, stream: usize, bytes: &[u8], whole_record: bool) {
        if bytes.is_empty() || self.truncated || self.error.is_some() {
            return;
        }
        // Reserve the stdout marker up front. Structured records are atomic;
        // raw chunks may use the last available payload bytes.
        let remaining = self.limit_bytes - TRUNCATION_MARKER.len() as u64 - self.persisted_bytes;
        let length = bytes.len().min(remaining as usize);
        if length < bytes.len() {
            self.truncated = true;
            if whole_record {
                return;
            }
        }
        if let Err(error) = self.files[stream].write(&bytes[..length]) {
            // Persistence failure must not leave a child blocked on a full pipe.
            // Keep draining both streams and report failure after wait/reap.
            self.error = Some(error);
        } else {
            self.persisted_bytes += length as u64;
        }
    }
}

struct Sink {
    file: Option<File>,
    sha256: Sha256,
    size_bytes: u64,
}
impl Sink {
    fn new(file: Option<File>) -> Self {
        Self {
            file,
            sha256: Sha256::new(),
            size_bytes: 0,
        }
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.write_all(bytes)?;
        }
        self.sha256.update(bytes);
        self.size_bytes += bytes.len() as u64;
        Ok(())
    }
    fn metadata(self) -> CapturedFile {
        CapturedFile {
            size_bytes: self.size_bytes,
            sha256: format!("{:x}", self.sha256.finalize()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_records_are_atomic_and_marker_is_inside_the_shared_limit() {
        let root = tempfile::tempdir().unwrap();
        let stdout = root.path().join("stdout");
        let stderr = root.path().join("stderr");
        let events = root.path().join("events");
        let limit = TRUNCATION_MARKER.len() as u64 + 3;
        let capture = Capture::new(
            File::create(&stdout).unwrap(),
            File::create(stderr).unwrap(),
            Some(File::create(&events).unwrap()),
            limit,
        )
        .unwrap();
        let counts = capture.stdout(&b"{}\n"[..]).unwrap();
        let metadata = capture.finish(counts).unwrap();
        assert!(metadata.truncated);
        assert_eq!(metadata.persisted_bytes, limit);
        assert_eq!(metadata.event_counts.parsed, 1);
        assert_eq!(metadata.events.size_bytes, 0);
        assert!(std::fs::read(events).unwrap().is_empty());
        let expected = [b"{}\n".as_slice(), TRUNCATION_MARKER].concat();
        assert_eq!(std::fs::read(stdout).unwrap(), expected);
        assert_eq!(
            metadata.stdout.sha256,
            format!("{:x}", Sha256::digest(expected))
        );
    }

    #[test]
    fn too_small_a_limit_is_rejected_before_capture() {
        let result = Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            None,
            TRUNCATION_MARKER.len() as u64 - 1,
        );
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::InvalidInput));
    }
}
