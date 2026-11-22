//! Literal-argv local harness execution. Output goes straight to files, never
//! undrained pipes. Bounded log/event capture and cancellation are later stories.

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
        // Temporary regular files avoid pipe deadlock and unbounded in-memory capture.
        let mut output = tempfile::tempfile().map_err(execution_error)?;
        let spec = CommandSpec {
            program: self.binary.clone(),
            args: vec!["--version".into()],
        };
        let status = execute(
            &spec,
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
                "Fake harness version probe failed or exceeded 4 KiB.",
            ));
        }
        output.seek(SeekFrom::Start(0)).map_err(execution_error)?;
        let mut version = String::new();
        output
            .read_to_string(&mut version)
            .map_err(execution_error)?;
        let version = version.trim();
        if version != "fabric-fake-harness 1" {
            return Err(execution_error("Unsupported fake harness version."));
        }
        Ok(HarnessInfo {
            version: version.to_owned(),
        })
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        Ok(CommandSpec {
            program: self.binary.clone(),
            args: vec!["--prompt-file".into(), spec.prompt_file.into()],
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

pub mod fake;
