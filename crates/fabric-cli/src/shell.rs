use clap::Args;
use fabric_domain::{EnvironmentId, EnvironmentSelection, ErrorKind, FabricError};
use fabric_runner::pty::{Closed, Exchange, Limits, MAX_BYTES, POLL_MS, Size};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Args)]
pub struct Options {
    pub environment_id: EnvironmentId,
    /// Explicit logical owner in addition to the immutable effective UID.
    #[arg(long)]
    pub owner: String,
    #[arg(long, default_value = "local")]
    pub target: String,
    #[arg(long, default_value_t = 300)]
    pub idle_seconds: u32,
    #[arg(long, default_value_t = 3600)]
    pub max_seconds: u32,
}

fn terminal_error() -> FabricError {
    FabricError { kind: ErrorKind::InvalidArguments,
        message: "fabric shell requires an attached input/output terminal; capture is never enabled by Fabric".into() }
}

struct Terminal {
    saved: libc::termios,
    input_flags: i32,
    output_flags: i32,
}
impl Terminal {
    fn inspect() -> Result<Self, FabricError> {
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::isatty(0) } != 1
            || unsafe { libc::isatty(1) } != 1
            || unsafe { libc::tcgetattr(0, &mut saved) } != 0
        {
            return Err(terminal_error());
        }
        let input_flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
        let output_flags = unsafe { libc::fcntl(1, libc::F_GETFL) };
        if input_flags < 0 || output_flags < 0 {
            return Err(terminal_error());
        }
        Ok(Self {
            saved,
            input_flags,
            output_flags,
        })
    }
    fn raw(&self) -> Result<(), FabricError> {
        let mut raw = self.saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0
                || libc::fcntl(0, libc::F_SETFL, self.input_flags | libc::O_NONBLOCK) < 0
                || libc::fcntl(1, libc::F_SETFL, self.output_flags | libc::O_NONBLOCK) < 0
            {
                return Err(terminal_error());
            }
        }
        Ok(())
    }
    fn size() -> Result<Size, FabricError> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(0, libc::TIOCGWINSZ as _, &mut size) } < 0 {
            return Err(terminal_error());
        }
        let size = Size {
            rows: size.ws_row,
            columns: size.ws_col,
        };
        size.validate()?;
        Ok(size)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(0, libc::F_SETFL, self.input_flags);
            libc::fcntl(1, libc::F_SETFL, self.output_flags);
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}

pub fn run(root: Option<PathBuf>, options: Options) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let limits = Limits {
        idle_seconds: options.idle_seconds,
        max_seconds: options.max_seconds,
    };
    limits.validate()?;
    let terminal = Terminal::inspect()?;
    let mut size = Terminal::size()?;
    let interrupt = fabric_runner::ForegroundInterrupt::install()?;
    let cancellation = interrupt.token();
    let selection = EnvironmentSelection {
        environment_id: options.environment_id,
        owner: options.owner,
        interactive_exec_pty: true,
    };
    let mut session = fabric_core::environment::shell(
        &root,
        &selection,
        &options.target,
        size,
        limits,
        &cancellation,
    )?;
    terminal.raw()?;
    let start = Instant::now();
    let mut activity = start;
    let mut input = Vec::with_capacity(MAX_BYTES);
    let mut output = Vec::new();
    let mut written = 0;
    let mut eof = false;
    let mut closed = None;
    let mut exit_code = None;
    loop {
        if cancellation.is_cancelled()
            || start.elapsed() >= Duration::from_secs(limits.max_seconds.into())
            || activity.elapsed() >= Duration::from_secs(limits.idle_seconds.into())
        {
            session.close(true)?;
            return Err(fabric_runner::pty::error());
        }
        if written == output.len() {
            output.clear();
            written = 0;
            if let Some(reason) = closed {
                session.close(false)?;
                return if reason == Closed::Exited && exit_code == Some(0) {
                    Ok(())
                } else {
                    Err(fabric_runner::pty::error())
                };
            }
            if eof && input.is_empty() {
                return session.close(false);
            }
            let current = Terminal::size()?;
            let resize = if current != size {
                size = current;
                Some(size)
            } else {
                None
            };
            // One bounded request in flight. Unaccepted bytes remain in this one
            // buffer; output is drained before asking the target for more.
            let request = Exchange {
                input: std::mem::take(&mut input),
                resize,
            };
            let reply = session.exchange(&request)?;
            input = request.input;
            input.drain(..reply.accepted);
            if reply.accepted != 0 || !reply.output.is_empty() || resize.is_some() {
                activity = Instant::now();
            }
            output = reply.output;
            closed = reply.closed;
            exit_code = reply.exit_code;
        }
        let mut descriptors = [
            libc::pollfd {
                fd: 0,
                events: if !eof && input.len() < MAX_BYTES && closed.is_none() {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: if written < output.len() {
                    libc::POLLOUT
                } else {
                    0
                },
                revents: 0,
            },
        ];
        if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, POLL_MS) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(fabric_runner::pty::error());
        }
        if descriptors
            .iter()
            .any(|fd| fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0)
        {
            return Err(fabric_runner::pty::error());
        }
        if descriptors[1].revents & libc::POLLHUP != 0 {
            return Err(fabric_runner::pty::error());
        }
        if input.len() < MAX_BYTES && descriptors[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut bytes = [0u8; MAX_BYTES];
            let count =
                unsafe { libc::read(0, bytes.as_mut_ptr().cast(), MAX_BYTES - input.len()) };
            if count == 0 {
                eof = true;
            } else if count > 0 {
                input.extend_from_slice(&bytes[..count as usize]);
            } else if !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Err(fabric_runner::pty::error());
            }
        }
        if descriptors[1].revents & libc::POLLOUT != 0 {
            let count = unsafe {
                libc::write(1, output[written..].as_ptr().cast(), output.len() - written)
            };
            if count > 0 {
                written += count as usize;
            } else if count == 0
                || !matches!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                )
            {
                return Err(fabric_runner::pty::error());
            }
        }
    }
}
