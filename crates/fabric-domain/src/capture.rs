//! Bounded output evidence, not a source artifact manifest or deletion authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventCounts {
    pub parsed: u64,
    pub invalid: u64,
    pub oversized: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Started {
        pid: u32,
    },
    Output {
        stream: String,
        text: String,
    },
    Tool {
        name: String,
        phase: String,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        cost_usd: Option<f64>,
    },
    Completed {
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        success: bool,
    },
    Failure {
        code: String,
        message: String,
    },
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
    /// Observed normalized events, including lifecycle and events dropped by the byte cap.
    /// This is a live cursor, not a promise that every event is persisted for replay.
    #[serde(default)]
    pub normalized_event_count: u64,
    #[serde(default)]
    pub native_failure: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_capture_metadata_defaults_normalized_evidence_and_usage_stays_unknown() {
        let metadata: CaptureMetadata = serde_json::from_value(serde_json::json!({
            "limit_bytes":1024,"persisted_bytes":0,"truncated":false,
            "stdout_bytes_seen":0,"stderr_bytes_seen":0,
            "stdout":{"size_bytes":0,"sha256":""},
            "stderr":{"size_bytes":0,"sha256":""},
            "events":{"size_bytes":0,"sha256":""},
            "event_counts":{"parsed":0,"invalid":0,"oversized":0}
        }))
        .unwrap();
        assert_eq!(metadata.normalized_event_count, 0);
        assert!(metadata.native_failure.is_none());
        let event = AgentEvent::Usage {
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cost_usd: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "usage");
        assert!(value["cost_usd"].is_null());
        assert_eq!(serde_json::from_value::<AgentEvent>(value).unwrap(), event);
    }
}
