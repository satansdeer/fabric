//! Owned-child supervision. Cancellation contains no PID and cannot adopt a DB record.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fabric_domain::{ErrorKind, FabricError};

use crate::execution_error;

#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    foreground_generation: u64,
    storage_stop: Arc<std::sync::atomic::AtomicU8>,
    deadline: Option<Instant>,
}
impl CancellationToken {
    /// Shares user cancellation but keeps storage termination private to one worker.
    pub fn worker_token(&self) -> Self {
        Self {
            cancelled: Arc::clone(&self.cancelled),
            foreground_generation: self.foreground_generation,
            storage_stop: Arc::default(),
            deadline: self.deadline,
        }
    }

    pub fn stop_for_storage(&self, reason: StopReason) {
        let value = match reason {
            StopReason::WorkspaceLimit => 1,
            StopReason::StoragePressure => 2,
            StopReason::StorageMeterFailed => 3,
            _ => return,
        };
        let _ = self
            .storage_stop
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn storage_reason(&self) -> Option<StopReason> {
        match self.storage_stop.load(Ordering::Acquire) {
            1 => Some(StopReason::WorkspaceLimit),
            2 => Some(StopReason::StoragePressure),
            3 => Some(StopReason::StorageMeterFailed),
            _ => None,
        }
    }
    /// Bounds only this clone; deadline expiry never cancels sibling workers.
    pub(crate) fn with_deadline(&self, deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            ..self.clone()
        }
    }

    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        if self.foreground_generation != 0
            && self.foreground_generation == HANDLER_GENERATION.load(Ordering::Acquire)
            && HANDLER_ACTIVE.load(Ordering::Acquire)
            && INTERRUPTED.load(Ordering::Acquire)
        {
            self.cancel();
        }
        self.cancelled.load(Ordering::Acquire) || self.deadline_expired()
    }
}

#[derive(Clone)]
pub struct ExecutionLimits {
    pub timeout: Duration,
    pub term_grace: Duration,
}
impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3600),
            term_grace: Duration::from_millis(250),
        }
    }
}
impl ExecutionLimits {
    pub fn validate(&self) -> Result<(), FabricError> {
        if self.timeout.is_zero()
            || Instant::now().checked_add(self.timeout).is_none()
            || self.term_grace > Duration::from_secs(30)
        {
            return Err(FabricError {
                kind: ErrorKind::InvalidArguments,
                message: "Timeout must be positive and representable; TERM grace must be at most 30 seconds.".into(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Timeout,
    Cancelled,
    WorkspaceLimit,
    StoragePressure,
    StorageMeterFailed,
}
impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::WorkspaceLimit => "workspace_limit",
            Self::StoragePressure => "storage_pressure",
            Self::StorageMeterFailed => "storage_meter_failed",
        }
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static HANDLER_ACTIVE: AtomicBool = AtomicBool::new(false);
static HANDLER_GENERATION: AtomicU64 = AtomicU64::new(0);
extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Release);
}

/// Scope to the foreground CLI invocation; restore the caller's signal dispositions.
pub struct ForegroundInterrupt {
    previous_sigint: libc::sigaction,
    previous_sigterm: libc::sigaction,
    token: CancellationToken,
}
impl ForegroundInterrupt {
    pub fn install() -> Result<Self, FabricError> {
        HANDLER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| execution_error("A foreground interrupt handler is already installed."))?;
        INTERRUPTED.store(false, Ordering::Release);
        let generation = HANDLER_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        // SAFETY: initialized structures, valid pointers; the handler only stores an atomic flag.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous_sigint = action;
        let mut previous_sigterm = action;
        action.sa_sigaction = interrupt as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        if unsafe { libc::sigaction(libc::SIGINT, &action, &mut previous_sigint) } == -1 {
            HANDLER_ACTIVE.store(false, Ordering::Release);
            return Err(execution_error(io::Error::last_os_error()));
        }
        if unsafe { libc::sigaction(libc::SIGTERM, &action, &mut previous_sigterm) } == -1 {
            let failure = io::Error::last_os_error();
            // SAFETY: SIGINT was installed above; roll it back before releasing exclusivity.
            unsafe {
                libc::sigaction(libc::SIGINT, &previous_sigint, std::ptr::null_mut());
            }
            HANDLER_ACTIVE.store(false, Ordering::Release);
            return Err(execution_error(failure));
        }
        Ok(Self {
            previous_sigint,
            previous_sigterm,
            token: CancellationToken {
                foreground_generation: generation,
                ..CancellationToken::default()
            },
        })
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}
impl Drop for ForegroundInterrupt {
    fn drop(&mut self) {
        if INTERRUPTED.load(Ordering::Acquire) {
            self.token.cancel();
        }
        // SAFETY: restore the disposition captured by this exclusive live guard.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.previous_sigterm, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.previous_sigint, std::ptr::null_mut());
        }
        HANDLER_ACTIVE.store(false, Ordering::Release);
    }
}

pub(crate) struct ProcessGroup {
    pub child: Child,
    reaped: bool,
    launched: Instant,
}
impl ProcessGroup {
    pub fn new(child: Child, launched: Instant) -> Self {
        Self {
            child,
            reaped: false,
            launched,
        }
    }

    pub(crate) fn signal(&mut self, signal: i32, _exit_deadline: Instant) -> io::Result<()> {
        // SAFETY: the unreaped, owned group leader reserves this PID until our last signal.
        // Never consult persisted PID metadata or signal after wait/reap.
        let result = unsafe { libc::kill(-(self.child.id() as i32), signal) };
        if result == -1 {
            let error = io::Error::last_os_error();
            // Darwin removes an exiting process from group lookup before waitid
            // can observe its zombie. EPERM is benign only after owned, unreaped
            // exit evidence arrives within the termination grace.
            #[cfg(target_os = "macos")]
            if error.raw_os_error() == Some(libc::EPERM) {
                return confirm_group_exit(error, _exit_deadline, || self.exited()).map_err(
                    |error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "kill(-{}, {signal}): {error}; owned-child exit not verified within termination grace",
                                self.child.id()
                            ),
                        )
                    },
                );
            }
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(io::Error::new(
                    error.kind(),
                    format!("kill(-{}, {signal}): {error}", self.child.id()),
                ));
            }
        }
        Ok(())
    }

    fn exit_info(&mut self) -> io::Result<libc::siginfo_t> {
        // WNOWAIT observes exit without releasing the PID/group identity anchor.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Err(error);
            }
            // Another reaper removed our ownership anchor. Never signal a possibly reused PID.
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
            }
            return Err(error);
        }
        Ok(info)
    }

    pub(crate) fn exited(&mut self) -> io::Result<bool> {
        let info = match self.exit_info() {
            Ok(info) => info,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(false),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "waitid(P_PID, {}, WEXITED|WNOHANG|WNOWAIT): {error}",
                        self.child.id()
                    ),
                ));
            }
        };
        #[cfg(target_os = "macos")]
        let pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let pid = unsafe { info.si_pid() };
        Ok(pid != 0
            && matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ))
    }

    /// One non-reaping observation; never release the group leader's PID anchor.
    pub(crate) fn timeout_snapshot(&mut self) -> serde_json::Value {
        let pid = self.child.id();
        let exit = match self.exit_info() {
            Ok(info) => {
                #[cfg(target_os = "macos")]
                let (observed_pid, status) = (info.si_pid, info.si_status);
                #[cfg(not(target_os = "macos"))]
                let (observed_pid, status) = unsafe { (info.si_pid(), info.si_status()) };
                if observed_pid == 0 {
                    serde_json::json!({"state":"not_exited"})
                } else {
                    match info.si_code {
                        libc::CLD_EXITED => serde_json::json!({"state":"exited","code":status}),
                        libc::CLD_KILLED | libc::CLD_DUMPED => {
                            serde_json::json!({"state":"signaled","signal":status})
                        }
                        libc::CLD_STOPPED | libc::CLD_TRAPPED => {
                            serde_json::json!({"state":"stopped","signal":status})
                        }
                        _ => {
                            serde_json::json!({"state":"UNKNOWN","reason":"unexpected_waitid_code","code":info.si_code})
                        }
                    }
                }
            }
            Err(error) => serde_json::json!({
                "state":"UNKNOWN","reason":"waitid_failed","errno":error.raw_os_error()
            }),
        };
        let pgid = if self.reaped {
            -1
        } else {
            unsafe { libc::getpgid(pid as i32) }
        };
        let pgid_errno = if pgid == -1 && !self.reaped {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        serde_json::json!({
            "pid":pid,
            "relationship":"owned_direct_child",
            "spawn_parent_pid":std::process::id(),
            "pgid":(pgid != -1).then_some(pgid),
            "pgid_state":if self.reaped { "UNKNOWN_anchor_lost" } else if pgid == -1 { "unavailable" } else { "observed" },
            "pgid_errno":pgid_errno,
            "descendants":"not_sampled",
            "try_wait":{"state":"UNKNOWN","reason":"not_called_preserve_group_pid_anchor"},
            "waitid_wnowait":exit
        })
    }

    pub fn supervise(
        &mut self,
        limits: &ExecutionLimits,
        cancellation: &CancellationToken,
        observed_alive: impl Fn(),
    ) -> Result<(ExitStatus, Option<StopReason>), FabricError> {
        self.supervise_observed(limits, cancellation, |_, stopping| {
            if !stopping {
                observed_alive();
            }
            Ok(())
        })
    }

    pub fn supervise_observed(
        &mut self,
        limits: &ExecutionLimits,
        cancellation: &CancellationToken,
        mut observed: impl FnMut(&mut Self, bool) -> Result<(), FabricError>,
    ) -> Result<(ExitStatus, Option<StopReason>), FabricError> {
        let start = self.launched;
        let mut next_observation = Duration::ZERO;
        let reason = loop {
            if let Some(reason) = cancellation.storage_reason() {
                break Some(reason);
            }
            if self.exited().map_err(execution_error)? {
                break None;
            }
            if start.elapsed() >= next_observation {
                observed(self, false)?;
                next_observation = start.elapsed() + Duration::from_millis(250);
            }
            if cancellation.is_cancelled() {
                break Some(StopReason::Cancelled);
            }
            if start.elapsed() >= limits.timeout {
                break Some(StopReason::Timeout);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        observed(self, true)?;
        Ok((self.stop_and_reap(limits.term_grace)?, reason))
    }

    pub(crate) fn stop_and_reap(&mut self, grace: Duration) -> Result<ExitStatus, FabricError> {
        if self.reaped {
            return Err(execution_error(
                "Owned process was already reaped; refusing to signal a reused PID",
            ));
        }
        // Also clean up background descendants after a natural leader exit.
        let deadline = Instant::now() + grace;
        self.signal(libc::SIGTERM, deadline)
            .map_err(execution_error)?;
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        self.signal(libc::SIGKILL, Instant::now() + grace)
            .map_err(execution_error)?;
        let status = self.child.wait().map_err(|error| {
            execution_error(format!("waitpid({}, 0): {error}", self.child.id()))
        })?;
        self.reaped = true;
        Ok(status)
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.reaped {
            let grace = Duration::from_millis(250);
            let deadline = Instant::now() + grace;
            let _ = self.signal(libc::SIGTERM, deadline);
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            let _ = self.signal(libc::SIGKILL, Instant::now() + grace);
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

#[cfg(target_os = "macos")]
fn confirm_group_exit(
    signal_error: io::Error,
    deadline: Instant,
    mut exited: impl FnMut() -> io::Result<bool>,
) -> io::Result<()> {
    loop {
        if exited()? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(signal_error);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}

/// Nonblocking pipe reads keep joining bounded even if trusted code leaves the group.
pub(crate) struct PipeReader<'a, R> {
    pipe: R,
    stopped: &'a AtomicBool,
    drain_time: Duration,
}
impl<'a, R: Read + AsRawFd> PipeReader<'a, R> {
    pub fn new(pipe: R, stopped: &'a AtomicBool) -> io::Result<Self> {
        // SAFETY: pipe owns a live descriptor. Preserve existing descriptor flags.
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pipe,
            stopped,
            drain_time: Duration::ZERO,
        })
    }
}
impl<R: Read> Read for PipeReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // Normalization runs between reads; it has its own execution deadline.
        // Charge only time inside reads, cumulatively even for a busy writer.
        let mut draining_since = None;
        loop {
            if self.stopped.load(Ordering::Acquire)
                && self.drain_time + draining_since.get_or_insert_with(Instant::now).elapsed()
                    >= Duration::from_millis(250)
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Capture pipe remained open after group termination; capture incomplete.",
                ));
            }
            match self.pipe.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                result => {
                    if let Some(start) = draining_since {
                        self.drain_time += start.elapsed();
                    }
                    return result;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::FromRawFd;

    #[cfg(target_os = "macos")]
    #[test]
    fn group_eperm_requires_eventual_owned_exit_and_does_not_reap() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let child = Command::new("/bin/sh")
            .args(["-c", "read ignored; exit 23"])
            .stdin(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut group = ProcessGroup::new(child, Instant::now());
        let mut release = group.child.stdin.take();
        assert!(!group.exited().unwrap());
        let error = confirm_group_exit(
            io::Error::from_raw_os_error(libc::EPERM),
            Instant::now(),
            || group.exited(),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));

        // Force the kernel boundary deterministically: the first real waitid
        // observes a live child; only then release it to exit.
        confirm_group_exit(
            io::Error::from_raw_os_error(libc::EPERM),
            Instant::now() + Duration::from_secs(2),
            || {
                let exited = group.exited()?;
                if release.is_some() {
                    assert!(!exited);
                    drop(release.take());
                }
                Ok(exited)
            },
        )
        .unwrap();
        assert!(group.exited().unwrap(), "exit observation must not reap");
        assert_eq!(
            group.stop_and_reap(Duration::ZERO).unwrap().code(),
            Some(23)
        );
    }

    #[test]
    fn stopped_capture_excludes_time_between_reads_from_drain_budget() {
        let mut descriptors = [0; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: each newly created descriptor is adopted exactly once.
        let reader = unsafe { File::from_raw_fd(descriptors[0]) };
        let mut writer = unsafe { File::from_raw_fd(descriptors[1]) };
        writer.write_all(b"ab").unwrap();
        drop(writer);
        let stopped = AtomicBool::new(true);
        let mut reader = PipeReader::new(reader, &stopped).unwrap();
        let mut byte = [0];
        assert_eq!(reader.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, *b"a");
        // Deterministically put consumer work beyond the old wall-clock budget.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(reader.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, *b"b");
        assert_eq!(reader.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn stopped_capture_returns_error_when_a_writer_keeps_pipe_open() {
        let mut descriptors = [0; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: each newly created descriptor is adopted exactly once.
        let reader = unsafe { File::from_raw_fd(descriptors[0]) };
        let _writer = unsafe { File::from_raw_fd(descriptors[1]) };
        let stopped = AtomicBool::new(true);
        let mut reader = PipeReader::new(reader, &stopped).unwrap();
        let start = Instant::now();
        assert_eq!(
            reader.read(&mut [0; 8]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
