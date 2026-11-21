//! Read-only local diagnostics. Discovery never executes the discovered tools.

use std::env;
use std::path::{Path, PathBuf};

use serde::Serialize;

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
    pub tools: [ToolReport; 2],
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
        tools: ["git", "omp"].map(|name| ToolReport {
            name,
            path: path.as_ref().and_then(|path| {
                env::split_paths(path).find_map(|directory| {
                    let candidate = directory.join(name);
                    if !is_executable(&candidate) {
                        return None;
                    }
                    if candidate.is_absolute() {
                        Some(candidate)
                    } else {
                        env::current_dir().ok().map(|cwd| cwd.join(candidate))
                    }
                })
            }),
        }),
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}
