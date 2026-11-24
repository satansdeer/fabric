//! Explicit target-side entry point; never launched by Core on the controller.
use std::io::Read;
use std::path::PathBuf;

/// PID1 only: reap adopted ordinary children, without signaling other sessions.
/// Owned agents remain responsible for their own cancellation and process groups.
#[cfg(target_os = "linux")]
fn init() -> Result<(), fabric_domain::FabricError> {
    if unsafe { libc::getpid() } != 1 {
        return Err(fabric_runner::execution_error("Init requires Linux PID1"));
    }
    unsafe {
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut signals, signal);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) != 0 {
            return Err(fabric_runner::execution_error("Init signal mask failed"));
        }
        loop {
            reap()?;
            let mut signal = 0;
            if libc::sigwait(&signals, &mut signal) != 0 {
                return Err(fabric_runner::execution_error("Init signal wait failed"));
            }
            if signal != libc::SIGCHLD {
                reap()?;
                return Ok(());
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn reap() -> Result<(), fabric_domain::FabricError> {
    loop {
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if pid > 0 {
            continue;
        }
        if pid == 0 {
            return Ok(());
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(()),
            _ => return Err(fabric_runner::execution_error("Init reap failed")),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn init() -> Result<(), fabric_domain::FabricError> {
    Err(fabric_runner::execution_error("Init requires Linux PID1"))
}
fn main() {
    let result = (|| -> Result<(), fabric_domain::FabricError> {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() == 1 && args[0] == "--init" {
            return init();
        }
        let initialize = args.len() == 3 && args[2] == "--initialize-home";
        if args.len() != 2 && !initialize {
            return Err(fabric_runner::execution_error(
                "Expected private HOME and work roots",
            ));
        }
        let mut line = Vec::new();
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut byte = [0];
            if stdin
                .read(&mut byte)
                .map_err(fabric_runner::execution_error)?
                != 1
                || line.len() >= 16384
            {
                return Err(fabric_runner::execution_error("Invalid remote binding"));
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        drop(stdin);
        let binding: serde_json::Value =
            serde_json::from_slice(&line).map_err(fabric_runner::execution_error)?;
        let instance =
            std::env::var("FABRIC_TARGET_INSTANCE").map_err(fabric_runner::execution_error)?;
        if initialize {
            fabric_core::remote_agent::initialize_persistent_home(
                std::path::Path::new(&args[0]),
                &binding,
            )?;
        }
        fabric_core::remote_agent::serve(
            PathBuf::from(&args[0]),
            PathBuf::from(&args[1]),
            binding,
            instance,
        )
    })();
    if result.is_err() {
        // Native errors and transport diagnostics can contain secrets. No raw stderr.
        eprintln!("Remote agent failed closed");
        std::process::exit(1);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn reaps_multiple_batches_and_keeps_operating() {
        // Direct children exercise the drain loop, not actual PID1 adoption.
        for _ in 0..2 {
            let mut children: Vec<_> = (0..3)
                .map(|_| std::process::Command::new("/bin/true").spawn().unwrap())
                .collect();
            for child in &children {
                assert!(
                    unsafe {
                        libc::waitid(
                            libc::P_PID,
                            child.id(),
                            &mut std::mem::zeroed(),
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    } == 0
                );
            }
            super::reap().unwrap();
            for child in &mut children {
                assert_eq!(
                    child.try_wait().unwrap_err().raw_os_error(),
                    Some(libc::ECHILD)
                );
            }
        }
    }
    #[test]
    fn refuses_non_pid1() {
        if unsafe { libc::getpid() } != 1 {
            assert!(super::init().is_err());
        }
    }
}
