//! Literal-argv local harness execution with bounded, checksummed capture.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

mod supervision;
pub use supervision::{CancellationToken, ExecutionLimits, ForegroundInterrupt, StopReason};
use supervision::{PipeReader, ProcessGroup};

use capture::Capture;
use fabric_domain::{ErrorKind, FabricError};

pub struct Execution {
    pub status: ExitStatus,
    pub stop_reason: Option<StopReason>,
    pub capture: std::io::Result<fabric_domain::CaptureMetadata>,
}

pub struct HarnessInfo {
    pub version: String,
}

pub struct AgentSpec<'a> {
    pub prompt_file: &'a Path,
    pub max_cost_usd: Option<f64>,
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
        if spec.max_cost_usd.is_some() {
            return Err(execution_error("Fake harness does not support max cost."));
        }
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
    capture: Capture,
    limits: &ExecutionLimits,
    cancellation: &CancellationToken,
    owner_lease: Option<&File>,
    started: impl FnOnce(u32) -> Result<(), FabricError>,
) -> Result<Execution, FabricError> {
    limits.validate()?;
    if cancellation.is_cancelled() {
        return Err(execution_error("Cancelled before harness launch."));
    }
    let launched = std::time::Instant::now();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .envs(spec.environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(lease) = owner_lease {
        let fd = lease.as_raw_fd();
        // Only this worker inherits the lease through exec. The parent's CLOEXEC
        // remains set, so unrelated probes cannot accidentally keep an owner alive.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|error| {
        execution_error(format!(
            "Cannot start harness {}: {error}",
            spec.program.display()
        ))
    })?;
    let mut group = ProcessGroup::new(child, launched);
    let stopped = AtomicBool::new(false);
    let stdout = PipeReader::new(group.child.stdout.take().expect("piped stdout"), &stopped)
        .map_err(execution_error)?;
    let stderr = PipeReader::new(group.child.stderr.take().expect("piped stderr"), &stopped)
        .map_err(execution_error)?;
    let (status, events) = std::thread::scope(|scope| {
        let out = scope.spawn(|| capture.stdout(stdout));
        let err = scope.spawn(|| capture.stderr(stderr));
        let status = started(group.child.id()).and_then(|()| group.supervise(limits, cancellation));
        // Stop the owned group before joining drains, including on persistence errors.
        drop(group);
        stopped.store(true, Ordering::Release);
        let events = out
            .join()
            .map_err(|_| execution_error("Stdout capture panicked."))?;
        let stderr = err
            .join()
            .map_err(|_| execution_error("Stderr capture panicked."))?;
        Ok::<_, FabricError>((status?, events.and_then(|events| stderr.map(|()| events))))
    })?;
    Ok(Execution {
        status: status.0,
        stop_reason: status.1,
        capture: events.and_then(|events| capture.finish(events)),
    })
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
    // Version probes use the same concurrent bounded draining path as runs.
    let mut output = tempfile::tempfile().map_err(execution_error)?;
    let capture = Capture::new(
        output.try_clone().map_err(execution_error)?,
        tempfile::tempfile().map_err(execution_error)?,
        None,
        4096 + capture::TRUNCATION_MARKER.len() as u64,
    )
    .map_err(execution_error)?;
    let result = execute(
        &CommandSpec {
            program: binary.to_owned(),
            args: vec!["--version".into()],
            environment,
        },
        Path::new("/"),
        capture,
        &ExecutionLimits {
            timeout: std::time::Duration::from_secs(5),
            ..ExecutionLimits::default()
        },
        &CancellationToken::default(),
        None,
        |_| Ok(()),
    )?;
    let metadata = result.capture.map_err(execution_error)?;
    if !result.status.success() || metadata.truncated || metadata.stdout_bytes_seen > 4096 {
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

pub mod capture;
pub mod jsonl;
pub mod omp;
pub use omp::OmpHarness;

pub mod fake;
