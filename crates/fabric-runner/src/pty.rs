//! Optional target PTY protocol. No run capture, transcript, credential or reconnect store.
use fabric_domain::{EnvironmentId, ErrorKind, FabricError};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const MAX_BYTES: usize = 4096;
// ponytail ceiling: one bounded exchange, 20ms polling rather than another
// async transport runtime; reconnect creates a fresh target-owned session.
pub const POLL_MS: i32 = 20;

pub fn error() -> FabricError {
    FabricError {
        kind: ErrorKind::Output,
        message: "PTY session failed; terminal content omitted from diagnostics".into(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub rows: u16,
    pub columns: u16,
}
impl Size {
    pub fn validate(self) -> Result<(), FabricError> {
        if self.rows == 0 || self.columns == 0 || self.rows > 1000 || self.columns > 1000 {
            return Err(error());
        }
        Ok(())
    }
    fn native(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub idle_seconds: u32,
    pub max_seconds: u32,
}
impl Limits {
    pub fn validate(self) -> Result<(), FabricError> {
        if self.idle_seconds == 0
            || self.idle_seconds > 900
            || self.max_seconds == 0
            || self.max_seconds > 14400
            || self.idle_seconds > self.max_seconds
        {
            return Err(error());
        }
        Ok(())
    }
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            idle_seconds: 300,
            max_seconds: 3600,
        }
    }
}

// Decimal strings avoid lossy JavaScript JSON inode numbers.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub device: String,
    pub inode: String,
}
impl Identity {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev().to_string(),
            inode: metadata.ino().to_string(),
        }
    }
    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        self.device.parse::<u64>().ok() == Some(metadata.dev())
            && self.inode.parse::<u64>().ok() == Some(metadata.ino())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenExec {
    pub version: u32,
    pub placement: String,
    pub environment_id: EnvironmentId,
    pub owner: String,
    pub owner_uid: u32,
    pub home: PathBuf,
    pub home_identity: Identity,
    pub namespace_identity: Identity,
    pub size: Size,
    pub limits: Limits,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opened {
    pub opened: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    pub input: Vec<u8>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub resize: Option<Size>,
}
impl Exchange {
    pub fn validate(&self) -> Result<(), FabricError> {
        if self.input.len() > MAX_BYTES {
            return Err(error());
        }
        if let Some(size) = self.resize {
            size.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Closed {
    Exited,
    Closed,
    Cancelled,
    IdleTimeout,
    MaxDuration,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub accepted: usize,
    pub output: Vec<u8>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub closed: Option<Closed>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub exit_code: Option<i32>,
}
impl Reply {
    pub fn validate(&self, offered: usize) -> Result<(), FabricError> {
        if self.accepted > offered
            || self.output.len() > MAX_BYTES
            || (self.closed.is_none() && self.exit_code.is_some())
        {
            return Err(error());
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Close {
    pub cancel: bool,
}

/// Target-side primitive, never invoked by Core. The target holds its own shared
/// environment lease through reap, including after the CLI loses its transport.
pub struct LocalPty {
    master: File,
    group: crate::supervision::ProcessGroup,
    _lease: File,
    limits: Limits,
    size: Size,
    start: Instant,
    activity: Instant,
    closed: Option<Closed>,
    exit_code: Option<i32>,
}
impl LocalPty {
    pub fn open(request: &OpenExec) -> Result<Self, FabricError> {
        request.size.validate()?;
        request.limits.validate()?;
        if request.version != 1
            || request.owner_uid != unsafe { libc::geteuid() }
            || request.home.file_name().is_none_or(|name| name != "tree")
            || request
                .home
                .parent()
                .and_then(|p| p.file_name())
                .is_none_or(|name| name != request.environment_id.as_str())
        {
            return Err(error());
        }
        let namespace = request
            .home
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(error)?;
        let private = |path: &std::path::Path| -> Result<File, FabricError> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(path)
                .map_err(|_| error())?;
            let metadata = file.metadata().map_err(|_| error())?;
            if metadata.uid() != request.owner_uid || metadata.mode() & 0o077 != 0 {
                return Err(error());
            }
            Ok(file)
        };
        let lease = private(namespace)?;
        if !request
            .namespace_identity
            .matches(&lease.metadata().map_err(|_| error())?)
            || unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0
        {
            return Err(error());
        }
        let home = private(&request.home)?;
        if !request
            .home_identity
            .matches(&home.metadata().map_err(|_| error())?)
        {
            return Err(error());
        }
        for relative in [
            ".config",
            ".cache",
            ".local",
            ".local/share",
            ".local/state",
        ] {
            private(&request.home.join(relative))?;
        }
        let mut master = -1;
        let mut slave = -1;
        let mut size = request.size.native();
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        } != 0
        {
            return Err(error());
        }
        // Both descriptors are now uniquely owned, even on subsequent failure.
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(error());
            }
        }
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(error());
        }
        let mut command = Command::new("/bin/sh");
        // ponytail: fixed POSIX shell, job control off; trusted same-UID code can
        // still escape its process group. This is not a process sandbox.
        command
            .args(["+m", "-i"])
            .env_clear()
            .env("HOME", &request.home)
            .env("HISTFILE", "/dev/null")
            .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("LC_CTYPE", "UTF-8")
            .current_dir(&request.home)
            .stdin(Stdio::from(slave.try_clone().map_err(|_| error())?))
            .stdout(Stdio::from(slave.try_clone().map_err(|_| error())?))
            .stderr(Stdio::from(slave));
        for (key, relative) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            command.env(key, request.home.join(relative));
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let start = Instant::now();
        let child = command.spawn().map_err(|_| error())?;
        Ok(Self {
            master,
            group: crate::supervision::ProcessGroup::new(child, start),
            _lease: lease,
            limits: request.limits,
            size: request.size,
            start,
            activity: start,
            closed: None,
            exit_code: None,
        })
    }

    pub fn tick(&mut self) -> Result<(), FabricError> {
        if self.closed.is_some() {
            return Ok(());
        }
        let reason = if self.group.exited().map_err(|_| error())? {
            Some(Closed::Exited)
        } else if self.start.elapsed() >= Duration::from_secs(self.limits.max_seconds.into()) {
            Some(Closed::MaxDuration)
        } else if self.activity.elapsed() >= Duration::from_secs(self.limits.idle_seconds.into()) {
            Some(Closed::IdleTimeout)
        } else {
            None
        };
        if let Some(reason) = reason {
            self.stop(reason)?;
        }
        Ok(())
    }

    fn stop(&mut self, reason: Closed) -> Result<(), FabricError> {
        if self.closed.is_none() {
            self.exit_code = self
                .group
                .stop_and_reap(Duration::from_millis(20))
                .map_err(|_| error())?
                .code();
            self.closed = Some(reason);
        }
        Ok(())
    }

    pub fn exchange(&mut self, request: &Exchange) -> Result<Reply, FabricError> {
        request.validate()?;
        self.tick()?;
        let mut accepted = 0;
        if self.closed.is_none() {
            if let Some(size) = request.resize {
                if size != self.size {
                    if unsafe {
                        libc::ioctl(
                            self.master.as_raw_fd(),
                            libc::TIOCSWINSZ as _,
                            &size.native(),
                        )
                    } < 0
                    {
                        return Err(error());
                    }
                    self.size = size;
                    self.activity = Instant::now();
                }
            }
            if !request.input.is_empty() {
                accepted = match self.master.write(&request.input) {
                    Ok(count) => count,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        0
                    }
                    Err(_) => return Err(error()),
                };
            }
        }
        let mut bytes = [0; MAX_BYTES];
        let count = match self.master.read(&mut bytes) {
            Ok(count) => count,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted
                    || e.raw_os_error() == Some(libc::EIO) =>
            {
                0
            }
            Err(_) => return Err(error()),
        };
        if accepted != 0 || count != 0 {
            self.activity = Instant::now();
        }
        // Drain queued output before advertising closure; every response stays bounded.
        Ok(Reply {
            accepted,
            output: bytes[..count].to_vec(),
            closed: if count == 0 { self.closed } else { None },
            exit_code: if count == 0 { self.exit_code } else { None },
        })
    }

    pub fn close(&mut self, cancel: bool) -> Result<Reply, FabricError> {
        self.stop(if cancel {
            Closed::Cancelled
        } else {
            Closed::Closed
        })?;
        Ok(Reply {
            accepted: 0,
            output: Vec::new(),
            closed: self.closed,
            exit_code: self.exit_code,
        })
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        let _ = self.stop(Closed::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_exchange_rejects_invalid_credit_bytes_and_truncated_metadata() {
        let reply = Reply {
            accepted: 2,
            output: vec![0, 195, 169, 3, 27],
            closed: None,
            exit_code: None,
        };
        assert!(reply.validate(1).is_err());
        assert!(
            Reply {
                accepted: 0,
                output: vec![0; MAX_BYTES + 1],
                closed: None,
                exit_code: None
            }
            .validate(0)
            .is_err()
        );
        assert!(
            Reply {
                accepted: 0,
                output: vec![],
                closed: None,
                exit_code: Some(0)
            }
            .validate(0)
            .is_err()
        );
        for value in [
            serde_json::json!({"accepted":0,"output":[256],"closed":null,"exit_code":null}),
            serde_json::json!({"accepted":0,"output":[],"exit_code":null}),
            serde_json::json!({"accepted":0,"output":[],"closed":null}),
        ] {
            assert!(serde_json::from_value::<Reply>(value).is_err());
        }
        assert!(
            Exchange {
                input: vec![0; MAX_BYTES + 1],
                resize: None
            }
            .validate()
            .is_err()
        );
    }
}
