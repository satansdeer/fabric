//! Content-free observations; activity is not evidence of task correctness.
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fabric_domain::AgentEvent;
use serde::{Deserialize, Serialize};

pub type SharedActivity = Arc<Mutex<Activity>>;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub frames: u64,
    pub normalized_events: u64,
    pub persisted_bytes: u64,
    pub capture_truncated: bool,
    pub last_output_at_ms: Option<u64>,
    pub last_event_at_ms: Option<u64>,
    pub tool_starts: u64,
    pub tool_ends: u64,
    pub last_tool_start_at_ms: Option<u64>,
    pub last_tool_end_at_ms: Option<u64>,
    pub last_tool_activity_at_ms: Option<u64>,
    pub current_tool: Option<Tool>,
    pub last_tool: Option<Tool>,
    pub process_started_at_ms: Option<u64>,
    pub process_exited_at_ms: Option<u64>,
    pub process_alive_observed_at_ms: Option<u64>,
}

// Never reflect arbitrary native names, arguments, results or errors in diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Read,
    Write,
    Edit,
    Bash,
    Grep,
    Glob,
    Task,
    CommandExecution,
    FileChange,
    Mcp,
    Other,
}

impl Activity {
    pub(crate) fn event(&mut self, event: &AgentEvent) {
        let now = timestamp_ms();
        self.normalized_events += 1;
        self.last_event_at_ms = Some(now);
        match event {
            AgentEvent::Started { .. } => {
                self.process_started_at_ms = Some(now);
                self.process_alive_observed_at_ms = Some(now);
            }
            AgentEvent::Completed { .. } => self.process_exited_at_ms = Some(now),
            _ => {}
        }
        if let AgentEvent::Tool { name, phase } = event {
            let tool = match name.as_str() {
                "read" => Tool::Read,
                "write" => Tool::Write,
                "edit" => Tool::Edit,
                "bash" => Tool::Bash,
                "grep" => Tool::Grep,
                "glob" => Tool::Glob,
                "task" => Tool::Task,
                "command_execution" => Tool::CommandExecution,
                "file_change" => Tool::FileChange,
                "mcp_tool_call" => Tool::Mcp,
                _ => Tool::Other,
            };
            self.last_tool = Some(tool);
            self.last_tool_activity_at_ms = Some(now);
            match phase.as_str() {
                "start" => {
                    self.tool_starts += 1;
                    self.last_tool_start_at_ms = Some(now);
                    self.current_tool = Some(tool);
                }
                "end" => {
                    self.tool_ends += 1;
                    self.last_tool_end_at_ms = Some(now);
                    self.current_tool = None;
                }
                _ => {}
            }
        }
    }
}

pub fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
