//! Independently installed reference plugins; not linked into Fabric Core.
pub use fabric_runner::{
    AgentSpec, CommandSpec, HarnessAdapter, HarnessInfo, NativeContext, NativeEvent,
    discover_executable, execution_error, planning, probe_help, probe_version, profile,
};
mod codex;
mod omp;
pub use codex::CodexHarness;
pub use omp::OmpHarness;
