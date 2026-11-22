//! Local diagnostics and durable workspace ownership orchestration.

use std::env;
use std::path::PathBuf;

use serde::Serialize;

pub mod api;
mod artifacts;
pub mod inspect;
pub mod run;
pub mod workspace;

#[derive(Debug, Serialize)]
pub struct PlatformReport {
    pub os: &'static str,
    pub arch: &'static str,
    pub supported: bool,
    pub primary_supported_os: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ToolReport {
    pub name: &'static str,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub platform: PlatformReport,
    pub tools: Vec<ToolReport>,
}

pub fn doctor() -> DoctorReport {
    let path = env::var_os("PATH");
    DoctorReport {
        platform: PlatformReport {
            os: env::consts::OS,
            arch: env::consts::ARCH,
            supported: cfg!(target_os = "macos"),
            primary_supported_os: "macos",
        },
        tools: std::iter::once("git")
            .chain(fabric_runner::HarnessRegistry::default().doctor_executables())
            .map(|name| ToolReport {
                name,
                path: path
                    .as_ref()
                    .and_then(|path| fabric_runner::discover_executable(name, path)),
            })
            .collect(),
    }
}
