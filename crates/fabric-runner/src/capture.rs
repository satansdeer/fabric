//! One shared byte budget for raw streams, structured JSONL and the marker.
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::sync::Mutex;

use fabric_domain::{AgentEvent, CaptureMetadata, CapturedFile, EventCounts};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::jsonl::{Event, Events};
use crate::{EventFormat, HarnessAdapter, NativeEvent};

pub const TRUNCATION_MARKER: &[u8] = b"\n[FABRIC CAPTURE TRUNCATED]\n";

/// Files must be freshly created private outputs. `None` disables event decoding
/// for version probes; ordinary runs always supply an events.jsonl file.
pub struct Capture<'a> {
    state: Mutex<State>,
    decode_events: bool,
    harness: Option<&'a dyn HarnessAdapter>,
    cancellation: crate::CancellationToken,
}

impl<'a> Capture<'a> {
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
            harness: None,
            cancellation: crate::CancellationToken::default(),
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
                activity: None,
                normalized_event_count: 0,
                native_failure: None,
                normalization_timed_out: false,
            }),
        })
    }

    pub fn with_harness(mut self, harness: &'a dyn HarnessAdapter) -> Self {
        self.harness = Some(harness);
        self
    }
    pub(crate) fn with_cancellation(mut self, cancellation: crate::CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub(crate) fn normalization_timed_out(&self) -> bool {
        self.state.lock().unwrap().normalization_timed_out
    }

    pub fn with_activity(self, activity: crate::activity::SharedActivity) -> Self {
        self.state.lock().unwrap().activity = Some(activity);
        self
    }

    pub(crate) fn lifecycle(&self, event: AgentEvent) {
        if self.harness.is_some() && self.decode_events {
            let mut state = self.state.lock().unwrap();
            state.normalized(&event);
        }
    }

    pub(crate) fn completed(
        &self,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        success: bool,
    ) {
        if self.harness.is_some() && self.decode_events {
            let mut state = self.state.lock().unwrap();
            let success = success && state.native_failure.is_none();
            state.normalized(&AgentEvent::Completed {
                exit_code,
                exit_signal,
                success,
            });
        }
    }

    pub(crate) fn observe_alive(&self) {
        if let Some(activity) = &self.state.lock().unwrap().activity {
            activity.lock().unwrap().process_alive_observed_at_ms =
                Some(crate::activity::timestamp_ms());
        }
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
        let format = self
            .harness
            .map_or(EventFormat::JsonLines, HarnessAdapter::event_format);
        let mut events = Events::with_format(BufReader::new(reader), format);
        // Reuse one bounded serialization buffer, never retain event values.
        let mut encoded = Vec::new();
        let mut normalization_failed = false;
        while let Some(event) = events.next() {
            let event = event?;
            let (status, value, text) = match &event {
                Event::Parsed(value) => {
                    counts.parsed += 1;
                    ("parsed", Some(value), None)
                }
                Event::Text(text) => {
                    counts.parsed += 1;
                    ("text", None, Some(text.as_str()))
                }
                Event::Invalid => {
                    counts.invalid += 1;
                    ("invalid", None, None)
                }
                Event::Oversized => {
                    counts.oversized += 1;
                    ("oversized", None, None)
                }
            };
            // Keep decoding and inspecting bounded frames after persistence is full.
            let normalized = if normalization_failed || self.cancellation.is_cancelled() {
                None
            } else {
                let result = match (self.harness, &event) {
                    (Some(harness), Event::Parsed(value)) => {
                        harness.try_normalize_event(NativeEvent::Json(value), &self.cancellation)
                    }
                    (Some(harness), Event::Text(text)) => {
                        harness.try_normalize_event(NativeEvent::Text(text), &self.cancellation)
                    }
                    _ => Ok(None),
                };
                match result {
                    Ok(event) => event,
                    Err(_) => {
                        normalization_failed = true;
                        if self.cancellation.is_cancelled() {
                            None
                        } else {
                            Some(AgentEvent::Failure {
                                code: "external-plugin-normalization".into(),
                                message:
                                    "External harness normalization failed; raw capture continues."
                                        .into(),
                            })
                        }
                    }
                }
            };
            let mut state = self.state.lock().unwrap();
            if self.harness.is_some() && self.cancellation.deadline_expired() {
                state.normalization_timed_out = true;
            }
            if let Some(activity) = &state.activity {
                activity.lock().unwrap().frames += 1;
            }
            let sequence = normalized.as_ref().map(|event| state.observe(event));
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
                    text,
                    event: normalized.as_ref(),
                    sequence,
                },
            )?;
            encoded.push(b'\n');
            state.persist(2, &encoded, true);
            state.publish();
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
        state.publish();
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
            normalized_event_count: state.normalized_event_count,
            native_failure: state.native_failure,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'a AgentEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
}

struct RawReader<'a, 'h, R> {
    pipe: R,
    capture: &'a Capture<'h>,
    stream: usize,
}
impl<R: Read> Read for RawReader<'_, '_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = self.pipe.read(buffer)?;
        let mut state = self.capture.state.lock().unwrap();
        state.seen[self.stream] += length as u64;
        if length > 0 {
            if let Some(activity) = &state.activity {
                activity.lock().unwrap().last_output_at_ms = Some(crate::activity::timestamp_ms());
            }
        }
        state.persist(self.stream, &buffer[..length], false);
        state.publish();
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
    normalized_event_count: u64,
    native_failure: Option<String>,
    normalization_timed_out: bool,
    activity: Option<crate::activity::SharedActivity>,
}
impl State {
    fn observe(&mut self, event: &AgentEvent) -> u64 {
        self.normalized_event_count += 1;
        if let Some(activity) = &self.activity {
            activity.lock().unwrap().event(event);
        }
        if let AgentEvent::Failure { code, .. } = event {
            if self.native_failure.is_none() {
                self.native_failure = Some(code.clone());
            }
        }
        self.normalized_event_count
    }

    fn publish(&self) {
        if let Some(activity) = &self.activity {
            let mut activity = activity.lock().unwrap();
            activity.stdout_bytes = self.seen[0];
            activity.stderr_bytes = self.seen[1];
            activity.persisted_bytes = self.persisted_bytes;
            activity.capture_truncated = self.truncated;
        }
    }
    fn normalized(&mut self, event: &AgentEvent) {
        let sequence = self.observe(event);
        if self.truncated || self.error.is_some() {
            return;
        }
        let record = Record {
            line_number: 0,
            line_bytes: 0,
            status: "normalized",
            value: None,
            text: None,
            event: Some(event),
            sequence: Some(sequence),
        };
        match serde_json::to_vec(&record) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                self.persist(2, &bytes, true);
            }
            Err(error) => self.error = Some(io::Error::other(error)),
        }
        self.publish();
    }
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

    #[test]
    fn bounded_frames_keep_unknown_and_malformed_evidence_without_inventing_completion() {
        let harness = crate::FakeAHarness::new("/unused".into()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events");
        let capture = Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            Some(File::create(&path).unwrap()),
            4 * 1024 * 1024,
        )
        .unwrap()
        .with_harness(&harness);
        let mut bytes = b"{\"kind\":\"unknown\",\"data\":1}\nnot-json\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', crate::jsonl::MAX_LINE_BYTES + 1));
        bytes.extend_from_slice(b"\n{\"kind\":\"output\",\"body\":\"ok\"}\n{\"kind\":");
        let counts = capture.stdout(bytes.as_slice()).unwrap();
        let metadata = capture.finish(counts).unwrap();
        assert_eq!(
            metadata.event_counts,
            EventCounts {
                parsed: 2,
                invalid: 2,
                oversized: 1
            }
        );
        assert_eq!(metadata.normalized_event_count, 1);
        assert!(metadata.native_failure.is_none());
        let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 5);
        assert_eq!(records[0]["value"]["data"], 1);
        assert_eq!(records[2]["status"], "oversized");
        assert_eq!(records[3]["event"]["type"], "output");
        assert!(
            records
                .iter()
                .all(|record| record["event"]["type"] != "completed")
        );
    }

    #[test]
    fn plaintext_is_bounded_and_missing_usage_is_not_fabricated() {
        let harness = crate::FakeBHarness::new("/unused".into()).unwrap();
        let capture = Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            Some(tempfile::tempfile().unwrap()),
            1_000_000,
        )
        .unwrap()
        .with_harness(&harness);
        let mut bytes = b"OUTPUT hi\r\nTOOL start fixture-write\nunrecognized\n\xff\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 64 * 1024));
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(b'x', 64 * 1024 + 1));
        bytes.extend_from_slice(b"\nTOOL end fixture-write");
        let counts = capture.stdout(bytes.as_slice()).unwrap();
        let metadata = capture.finish(counts).unwrap();
        assert_eq!(
            metadata.event_counts,
            EventCounts {
                parsed: 5,
                invalid: 1,
                oversized: 1
            }
        );
        assert_eq!(metadata.normalized_event_count, 3);
        assert!(metadata.native_failure.is_none());
        assert!(!metadata.truncated);
    }

    #[test]
    fn metadata_continues_after_raw_truncation_without_native_content() {
        let harness = crate::FakeBHarness::new("/unused".into()).unwrap();
        let activity = crate::activity::SharedActivity::default();
        let capture = Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            Some(tempfile::tempfile().unwrap()),
            TRUNCATION_MARKER.len() as u64,
        )
        .unwrap()
        .with_harness(&harness)
        .with_activity(activity.clone());
        capture
            .stdout(&b"TOOL start private-tool-name\n"[..])
            .unwrap();
        let first = *activity.lock().unwrap();
        assert!(first.capture_truncated);
        capture
            .stdout(&b"OUTPUT private-model-content\nTOOL end private-tool-name\n"[..])
            .unwrap();
        capture.stderr(&b"private-native-error"[..]).unwrap();
        let last = *activity.lock().unwrap();
        assert!(last.stdout_bytes > first.stdout_bytes);
        assert!(last.stderr_bytes > first.stderr_bytes);
        assert_eq!(last.normalized_events, 3);
        assert_eq!((last.tool_starts, last.tool_ends), (1, 1));
        assert!(last.current_tool.is_none());
        assert!(last.last_tool_activity_at_ms.is_some());
        assert!(last.persisted_bytes <= TRUNCATION_MARKER.len() as u64);
        let json = serde_json::to_string(&last).unwrap();
        assert!(json.contains("\"other\""));
        assert!(!json.contains("private-"));
    }
}
