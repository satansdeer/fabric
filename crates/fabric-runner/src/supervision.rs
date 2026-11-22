//! Owned-child supervision. Cancellation contains no PID and cannot adopt a DB record.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fabric_domain::FabricError;

use crate::execution_error;

#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    foreground_generation: u64,
    storage_stop: Arc<std::sync::atomic::AtomicU8>,
}
impl CancellationToken {
    /// Shares user cancellation but keeps storage termination private to one worker.
    pub fn worker_token(&self) -> Self {
        Self {
            cancelled: Arc::clone(&self.cancelled),
            foreground_generation: self.foreground_generation,
            storage_stop: Arc::default(),
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
        self.cancelled.load(Ordering::Acquire)
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
            return Err(execution_error(
                "Timeout must be positive and representable; TERM grace must be at most 30 seconds.",
            ));
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

    fn signal(&mut self, signal: i32) -> io::Result<()> {
        // SAFETY: the unreaped, owned group leader reserves this PID until our last signal.
        // Never consult persisted PID metadata or signal after wait/reap.
        let result = unsafe { libc::kill(-(self.child.id() as i32), signal) };
        if result == -1 {
            let error = io::Error::last_os_error();
            // Darwin skips zombies when signalling a group and returns EPERM if
            // none remain signalable. Confirm the owned leader exited first;
            // EPERM while it is live still fails closed.
            #[cfg(target_os = "macos")]
            if error.raw_os_error() == Some(libc::EPERM) && self.exited()? {
                return Ok(());
            }
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    fn exited(&mut self) -> io::Result<bool> {
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
                return Ok(false);
            }
            // Another reaper removed our ownership anchor. Never signal a possibly reused PID.
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
            }
            return Err(error);
        }
        #[cfg(target_os = "macos")]
        let pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let pid = unsafe { info.si_pid() };
        Ok(pid != 0)
    }

    pub fn supervise(
        &mut self,
        limits: &ExecutionLimits,
        cancellation: &CancellationToken,
    ) -> Result<(ExitStatus, Option<StopReason>), FabricError> {
        let start = self.launched;
        let reason = loop {
            if let Some(reason) = cancellation.storage_reason() {
                break Some(reason);
            }
            if self.exited().map_err(execution_error)? {
                break None;
            }
            if cancellation.is_cancelled() {
                break Some(StopReason::Cancelled);
            }
            if start.elapsed() >= limits.timeout {
                break Some(StopReason::Timeout);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // Also clean up background descendants after a natural leader exit.
        self.signal(libc::SIGTERM).map_err(execution_error)?;
        std::thread::sleep(limits.term_grace);
        self.signal(libc::SIGKILL).map_err(execution_error)?;
        let status = self.child.wait().map_err(execution_error)?;
        self.reaped = true;
        Ok((status, reason))
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.signal(libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(250));
            let _ = self.signal(libc::SIGKILL);
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

/// Nonblocking pipe reads keep joining bounded even if trusted code leaves the group.
pub(crate) struct PipeReader<'a, R> {
    pipe: R,
    stopped: &'a AtomicBool,
    stopped_at: Option<Instant>,
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
            stopped_at: None,
        })
    }
}
impl<R: Read> Read for PipeReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stopped.load(Ordering::Acquire)
                && self.stopped_at.get_or_insert_with(Instant::now).elapsed()
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
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::FromRawFd;

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
