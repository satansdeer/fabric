//! Bounded output evidence, not a source artifact manifest or deletion authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventCounts {
    pub parsed: u64,
    pub invalid: u64,
    pub oversized: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapturedFile {
    pub size_bytes: u64,
    /// SHA-256 of exactly the persisted bytes, including any truncation marker.
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CaptureMetadata {
    pub limit_bytes: u64,
    /// Combined stdout.log + stderr.log + events.jsonl bytes, including markers.
    pub persisted_bytes: u64,
    pub truncated: bool,
    pub stdout_bytes_seen: u64,
    pub stderr_bytes_seen: u64,
    pub stdout: CapturedFile,
    pub stderr: CapturedFile,
    pub events: CapturedFile,
    /// Counts the entire drained stdout, even after persistence reaches its cap.
    pub event_counts: EventCounts,
}
