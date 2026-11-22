//! Literal-argv local harness execution with bounded JSONL decoding.
//! Total log caps and cancellation are later stories.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use fabric_domain::{ErrorKind, FabricError};

pub struct HarnessInfo {
    pub version: String,
}

pub struct AgentSpec<'a> {
    pub prompt_file: &'a Path,
}

pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Explicit selected environment, applied after clearing the parent environment.
    pub environment: Vec<(OsString, OsString)>,
}

pub trait HarnessAdapter {
    fn name(&self) -> &str;
    fn probe(&self) -> Result<HarnessInfo, FabricError>;
    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError>;
}

pub struct FakeHarness {
    binary: PathBuf,
}

impl FakeHarness {
    pub fn new(binary: PathBuf) -> Result<Self, FabricError> {
        if !binary.is_absolute() {
            return Err(execution_error(
                "Harness executable must be an absolute path.",
            ));
        }
        Ok(Self { binary })
    }
}

impl HarnessAdapter for FakeHarness {
    fn name(&self) -> &str {
        "fake"
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = probe_version(&self.binary, Vec::new())?;
        if version != "fabric-fake-harness 1" {
            return Err(execution_error("Unsupported fake harness version."));
        }
        Ok(HarnessInfo { version })
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        Ok(CommandSpec {
            program: self.binary.clone(),
            args: vec!["--prompt-file".into(), spec.prompt_file.into()],
            environment: Vec::new(),
        })
    }
}

/// Core must supply a freshly validated owned fork as cwd. The callback commits
/// RUNNING only after spawn; persistence failure kills/reaps the actual child.
pub fn execute(
    spec: &CommandSpec,
    cwd: &Path,
    stdout: File,
    stderr: File,
    started: impl FnOnce(u32) -> Result<(), FabricError>,
) -> Result<ExitStatus, FabricError> {
    let child = Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .envs(spec.environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| {
            execution_error(format!(
                "Cannot start harness {}: {error}",
                spec.program.display()
            ))
        })?;
    let mut group = ProcessGroup { child };
    started(group.child.id())?;
    group.child.wait().map_err(execution_error)
}

struct ProcessGroup {
    child: Child,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: this owned Child was spawned in its own process group. Never
        // signal a stale DB PID. Terminate background descendants after leader exit.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

fn execution_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Output,
        message: error.to_string(),
    }
}

fn probe_version(
    binary: &Path,
    environment: Vec<(OsString, OsString)>,
) -> Result<String, FabricError> {
    // Regular files avoid undrained pipes; retained bytes are capped in FAB-12.
    let mut output = tempfile::tempfile().map_err(execution_error)?;
    let status = execute(
        &CommandSpec {
            program: binary.to_owned(),
            args: vec!["--version".into()],
            environment,
        },
        Path::new("/"),
        output.try_clone().map_err(execution_error)?,
        File::options()
            .write(true)
            .open("/dev/null")
            .map_err(execution_error)?,
        |_| Ok(()),
    )?;
    if !status.success() || output.metadata().map_err(execution_error)?.len() > 4096 {
        return Err(execution_error(
            "Harness version probe failed or exceeded 4 KiB.",
        ));
    }
    output.seek(SeekFrom::Start(0)).map_err(execution_error)?;
    let mut version = String::new();
    output
        .take(4097)
        .read_to_string(&mut version)
        .map_err(execution_error)?;
    Ok(version.trim_end_matches(['\r', '\n']).to_owned())
}

/// Discover without executing anything; relative PATH entries resolve before cwd changes.
pub fn discover_executable(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).find_map(|directory| {
        let candidate = directory.join(name);
        let metadata = candidate.metadata().ok()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
        if candidate.is_absolute() {
            Some(candidate)
        } else {
            std::env::current_dir().ok().map(|cwd| cwd.join(candidate))
        }
    })
}

pub mod jsonl;
pub mod omp;
pub use omp::OmpHarness;

pub mod fake;
