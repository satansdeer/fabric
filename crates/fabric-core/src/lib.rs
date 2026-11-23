//! Local diagnostics and durable workspace ownership orchestration.
mod target;

use std::env;
use std::path::PathBuf;
pub mod manager;

use serde::Serialize;

pub mod api;
mod artifacts;
pub mod connector;
pub mod inspect;
pub mod logs;
pub mod onboarding;
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
    pub name: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub platform: PlatformReport,
    pub tools: Vec<ToolReport>,
}

pub fn doctor() -> Result<DoctorReport, fabric_domain::FabricError> {
    let path = env::var_os("PATH");
    Ok(DoctorReport {
        platform: PlatformReport {
            os: env::consts::OS,
            arch: env::consts::ARCH,
            supported: cfg!(target_os = "macos"),
            primary_supported_os: "macos",
        },
        tools: std::iter::once("git".to_owned())
            .chain(fabric_runner::HarnessRegistry::default().doctor_executables()?)
            .map(|name| ToolReport {
                path: path
                    .as_ref()
                    .and_then(|path| fabric_runner::discover_executable(&name, path)),
                name,
            })
            .collect(),
    })
}
