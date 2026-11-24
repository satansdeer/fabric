//! Literal-argv local harness execution with bounded, checksummed capture.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

pub mod controller;
pub mod pty;
mod supervision;
pub mod target;
pub use supervision::{CancellationToken, ExecutionLimits, ForegroundInterrupt, StopReason};
use supervision::{PipeReader, ProcessGroup};

use capture::Capture;
use fabric_domain::{
    AgentEvent, CapabilitySupport, ErrorKind, FabricError, HarnessCapabilityManifest, HarnessId,
    HarnessPlan, HarnessRequest, HarnessVersion,
};

pub struct Execution {
    pub status: ExitStatus,
    pub stop_reason: Option<StopReason>,
    pub capture: std::io::Result<fabric_domain::CaptureMetadata>,
    /// Post-spawn failure; status and finalized capture remain authoritative.
    pub failure: Option<FabricError>,
    /// Target observation or receipt failed, independently of a primary host error.
    pub target_failed: bool,
}

pub struct HarnessInfo {
    pub version: String,
}

/// Metadata-only description of the native context offered for local consent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeContext {
    pub profile: fabric_domain::AgentProfile,
    pub credential_paths: Vec<PathBuf>,
    pub credential_available: bool,
    pub exposure: String,
}

pub struct AgentSpec<'a> {
    pub profile: &'a fabric_domain::ProfileEvidence,
    pub prompt_file: &'a Path,
    pub max_cost_usd: Option<f64>,
    pub request: &'a HarnessRequest,
}

pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Explicit selected environment, applied after clearing the parent environment.
    pub environment: Vec<(OsString, OsString)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventFormat {
    JsonLines,
    TextLines,
}

pub enum NativeEvent<'a> {
    Json(&'a serde_json::Value),
    Text(&'a str),
}

pub trait HarnessAdapter: Sync {
    fn id(&self) -> HarnessId;
    fn probe(&self) -> Result<HarnessInfo, FabricError>;
    fn native_context(&self) -> Result<NativeContext, FabricError> {
        Err(FabricError {
            kind: ErrorKind::UnsupportedCapability,
            message: "This harness has no native-context onboarding adapter; use an explicit supported profile.".into(),
        })
    }
    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        HarnessVersion::validate(&info.version)?;
        Ok(HarnessCapabilityManifest::unsupported(
            self.id(),
            info.version.clone(),
            "none attested".into(),
        ))
    }
    fn plan(
        &self,
        request: &HarnessRequest,
        info: &HarnessInfo,
    ) -> Result<HarnessPlan, FabricError> {
        negotiate(request, self.capabilities(info)?, false)
    }
    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError>;
    fn event_format(&self) -> EventFormat {
        EventFormat::JsonLines
    }
    fn normalize_event(&self, _event: NativeEvent<'_>) -> Option<AgentEvent> {
        None
    }
    fn try_normalize_event(
        &self,
        event: NativeEvent<'_>,
        _cancellation: &CancellationToken,
    ) -> Result<Option<AgentEvent>, FabricError> {
        Ok(self.normalize_event(event))
    }
}

pub struct FakeHarness {
    binary: PathBuf,
}

impl FakeHarness {
    const SUPPORTED_VERSION: &'static str = "fabric-fake-harness 1";

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
    fn id(&self) -> HarnessId {
        "fake".parse().expect("registered harness ID")
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = probe_version(&self.binary, Vec::new())?;
        planning::check_version("fake", &version, Self::SUPPORTED_VERSION)?;
        Ok(HarnessInfo { version })
    }

    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        planning::local_manifest(
            self.id(),
            info,
            Self::SUPPORTED_VERSION,
            CapabilitySupport::FabricManaged,
        )
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        profile::revalidate(self, spec, Self::SUPPORTED_VERSION)?;
        Ok(CommandSpec {
            program: self.binary.clone(),
            args: vec!["--prompt-file".into(), spec.prompt_file.into()],
            environment: Vec::new(),
        })
    }

    fn normalize_event(&self, event: NativeEvent<'_>) -> Option<AgentEvent> {
        let NativeEvent::Json(value) = event else {
            return None;
        };
        match value["type"].as_str()? {
            "output" => Some(AgentEvent::Output {
                stream: "stdout".into(),
                text: value["text"].as_str()?.into(),
            }),
            "failure" => Some(AgentEvent::Failure {
                code: value["code"].as_str()?.into(),
                message: value["message"].as_str()?.into(),
            }),
            _ => None,
        }
    }
}

/// Validated launch inputs shared by direct and installed-target execution.
pub struct ExecutionRequest<'a> {
    pub spec: &'a CommandSpec,
    pub cwd: &'a Path,
    pub capture: Capture<'a>,
    pub limits: &'a ExecutionLimits,
    pub cancellation: &'a CancellationToken,
    pub owner_lease: Option<&'a File>,
}

/// Core must supply a freshly validated owned fork as cwd. The callback commits
/// RUNNING only after spawn; persistence failure kills/reaps the actual child.
pub fn execute(
    spec: &CommandSpec,
    cwd: &Path,
    capture: Capture<'_>,
    limits: &ExecutionLimits,
    cancellation: &CancellationToken,
    owner_lease: Option<&File>,
    started: impl FnOnce(u32) -> Result<(), FabricError>,
) -> Result<Execution, FabricError> {
    execute_observed(
        ExecutionRequest {
            spec,
            cwd,
            capture,
            limits,
            cancellation,
            owner_lease,
        },
        started,
        |_, _| Ok(()),
    )
    .and_then(|mut execution| match execution.failure.take() {
        Some(failure) => Err(failure),
        None => Ok(execution),
    })
}

pub(crate) fn execute_observed(
    request: ExecutionRequest<'_>,
    started: impl FnOnce(u32) -> Result<(), FabricError>,
    mut observed: impl FnMut(&mut ProcessGroup, bool) -> Result<(), FabricError>,
) -> Result<Execution, FabricError> {
    let ExecutionRequest {
        spec,
        cwd,
        capture,
        limits,
        cancellation,
        owner_lease,
    } = request;
    limits.validate()?;
    if cancellation.is_cancelled() {
        return Err(execution_error("Cancelled before harness launch."));
    }
    let launched = std::time::Instant::now();
    let capture = capture.with_cancellation(cancellation.with_deadline(launched + limits.timeout));
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C");
    // Keep private discovery roots alive through supervised termination and reap.
    let _runtime_home = if [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ]
    .into_iter()
    .all(|name| spec.environment.iter().any(|(key, _)| key == name))
    {
        None
    } else {
        Some(profile::private_environment(&mut command)?)
    };
    command
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
    capture.lifecycle(AgentEvent::Started {
        pid: group.child.id(),
    });
    let stopped = AtomicBool::new(false);
    let stdout = PipeReader::new(group.child.stdout.take().expect("piped stdout"), &stopped)
        .map_err(execution_error)?;
    let stderr = PipeReader::new(group.child.stderr.take().expect("piped stderr"), &stopped)
        .map_err(execution_error)?;
    let (mut status, failure, events) = std::thread::scope(|scope| {
        let out = scope.spawn(|| capture.stdout(stdout));
        let err = scope.spawn(|| capture.stderr(stderr));
        let mut observed_stop_reason = None;
        let status = started(group.child.id()).and_then(|()| {
            observed(&mut group, false)?;
            group.supervise_observed(limits, cancellation, |group, stopping| {
                if stopping {
                    observed_stop_reason = cancellation.storage_reason().or_else(|| {
                        if group.exited().ok() == Some(true) {
                            None
                        } else if cancellation.is_cancelled() {
                            Some(StopReason::Cancelled)
                        } else if launched.elapsed() >= limits.timeout {
                            Some(StopReason::Timeout)
                        } else {
                            None
                        }
                    });
                } else {
                    capture.observe_alive();
                }
                observed(group, stopping)
            })
        });
        // Retain the actual terminal outcome even when target observation failed.
        let (status, failure) = match status {
            Ok(status) => (status, None),
            Err(failure) => (
                (
                    group.stop_and_reap(limits.term_grace)?,
                    observed_stop_reason,
                ),
                Some(failure),
            ),
        };
        drop(group);
        stopped.store(true, Ordering::Release);
        let events = out
            .join()
            .map_err(|_| execution_error("Stdout capture panicked."))?;
        let stderr = err
            .join()
            .map_err(|_| execution_error("Stderr capture panicked."))?;
        Ok::<_, FabricError>((
            status,
            failure,
            events.and_then(|events| stderr.map(|()| events)),
        ))
    })?;
    if status.1.is_none() && capture.normalization_timed_out() {
        status.1 = Some(StopReason::Timeout);
    }
    capture.completed(
        status.0.code(),
        status.0.signal(),
        status.0.success() && status.1.is_none() && failure.is_none(),
    );
    Ok(Execution {
        status: status.0,
        stop_reason: status.1,
        capture: events.and_then(|events| capture.finish(events)),
        failure,
        target_failed: false,
    })
}

pub fn execution_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Output,
        message: error.to_string(),
    }
}

pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const PROBE_TERM_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

pub fn probe_version(
    binary: &Path,
    environment: Vec<(OsString, OsString)>,
) -> Result<String, FabricError> {
    probe_version_args(binary, vec!["--version".into()], environment)
}

fn probe_version_args(
    binary: &Path,
    args: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
) -> Result<String, FabricError> {
    Ok(probe_output(binary, args, environment, 4096)?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

/// Model-free CLI surface discovery, using the same isolation and process bounds as version probes.
pub fn probe_help(
    binary: &Path,
    environment: Vec<(OsString, OsString)>,
) -> Result<String, FabricError> {
    probe_output(binary, vec!["--help".into()], environment, 65536)
}

fn probe_output(
    binary: &Path,
    args: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    max_bytes: u64,
) -> Result<String, FabricError> {
    // Discovery probes use the same concurrent bounded draining path as runs.
    let mut output = tempfile::tempfile().map_err(execution_error)?;
    let capture = Capture::new(
        output.try_clone().map_err(execution_error)?,
        tempfile::tempfile().map_err(execution_error)?,
        None,
        max_bytes + capture::TRUNCATION_MARKER.len() as u64,
    )
    .map_err(execution_error)?;
    let result = execute(
        &CommandSpec {
            program: binary.to_owned(),
            args,
            environment,
        },
        Path::new("/"),
        capture,
        &ExecutionLimits {
            timeout: PROBE_TIMEOUT,
            term_grace: PROBE_TERM_GRACE,
        },
        &CancellationToken::default(),
        None,
        |_| Ok(()),
    )?;
    let metadata = result.capture.map_err(execution_error)?;
    if result.stop_reason == Some(StopReason::Timeout) {
        return Err(FabricError {
            kind: ErrorKind::Timeout,
            message: "Harness discovery command exceeded its 5s budget.".into(),
        });
    }
    if !result.status.success() || metadata.truncated || metadata.stdout_bytes_seen > max_bytes {
        return Err(execution_error(
            "Harness discovery probe failed or exceeded its output limit.",
        ));
    }
    output.seek(SeekFrom::Start(0)).map_err(execution_error)?;
    let mut version = String::new();
    output
        .take(max_bytes + 1)
        .read_to_string(&mut version)
        .map_err(execution_error)?;
    Ok(version)
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

pub mod activity;
pub mod capture;
pub mod jsonl;

pub mod fake;
mod fake_adapters;
pub use fake_adapters::{FakeAHarness, FakeBHarness};

pub mod plugin;
mod registry;
pub use registry::{HarnessRegistration, HarnessRegistry, RegistryOptions};

pub mod native_plugin;
pub mod planning;
pub mod profile;
pub use planning::negotiate;
