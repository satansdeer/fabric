//! Reference trusted-local driver. Core interprets these proposals with its own validated objects.
use fabric_runner::pty::{Close, Exchange, LocalPty, OpenExec, POLL_MS};
use fabric_runner::target::{Action, ForkMode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    placement: String,
    run_id: String,
    grant: String,
    actions: Vec<Action>,
    context: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentJob {
    placement: String,
    environment_id: fabric_domain::EnvironmentId,
    grant: String,
    actions: Vec<Action>,
    context: Value,
}

fn serve() -> Result<(), ()> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--fabric-plugin-stdio") || args.next().is_some() {
        return Err(());
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut frame = Vec::with_capacity(1024);
    let mut id = 1u64;
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let interrupt = fabric_runner::ForegroundInterrupt::install().map_err(|_| ())?;
    let cancellation = interrupt.token();
    let flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(input.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(());
    }
    let output_flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    if output_flags < 0
        || unsafe {
            libc::fcntl(
                output.as_raw_fd(),
                libc::F_SETFL,
                output_flags | libc::O_NONBLOCK,
            )
        } < 0
    {
        return Err(());
    }
    let mut pty: Option<LocalPty> = None;
    loop {
        frame.clear();
        loop {
            if cancellation.is_cancelled() {
                return Err(());
            }
            if let Some(session) = pty.as_mut() {
                session.tick().map_err(|_| ())?;
            }
            if frame.len() >= 65_536 {
                return Err(());
            }
            match input
                .by_ref()
                .take((65_537 - frame.len()) as u64)
                .read_until(b'\n', &mut frame)
            {
                Ok(0) => return Err(()),
                Ok(_) if frame.last() == Some(&b'\n') => break,
                Ok(_) => {}
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(()),
            }
            let mut fd = libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut fd, 1, POLL_MS);
            }
        }
        if frame.len() > 65_536 {
            return Err(());
        }
        let request: Request = serde_json::from_slice(&frame).map_err(|_| ())?;
        if request.jsonrpc != "2.0" || request.id != id {
            return Err(());
        }
        let shutdown = request.method == "shutdown";
        let result = if id == 1 {
            if request.method != "initialize" || request.params != json!({"protocol":1}) {
                return Err(());
            }
            json!({"protocol":1,"id":"local","version":"0.1.0","kind":"target"})
        } else if id == 2 {
            if request.method != "health" || request.params != json!({}) {
                return Err(());
            }
            json!({"ready":true})
        } else if shutdown {
            if request.params != json!({}) || !pending.is_empty() || pty.is_some() {
                return Err(());
            }
            json!({"ready":true})
        } else if request.method == "target.capabilities" {
            if request.params != json!({}) {
                return Err(());
            }
            json!({"placements":["local"],"operations":fabric_runner::target::OPERATIONS,"optional":["environment.v1","pty.v1"]})
        } else if request.method == "target.placement" {
            if request.params != json!({"placement":"local","backends":["host.local"]}) {
                return Err(());
            }
            json!({"backend":"host.local"})
        } else if request.method == "target.environment.capabilities" {
            if request.params != json!({"version":1}) {
                return Err(());
            }
            let mut caps = fabric_domain::EnvironmentCapabilities::local();
            caps.interactive_exec_pty = true;
            serde_json::to_value(caps).map_err(|_| ())?
        } else if request.method == "target.pty.openExec" {
            if pty.is_some() {
                return Err(());
            }
            let open: OpenExec = serde_json::from_value(request.params).map_err(|_| ())?;
            if open.placement != "local" {
                return Err(());
            }
            pty = Some(LocalPty::open(&open).map_err(|_| ())?);
            json!({"opened":true})
        } else if request.method == "target.pty.exchange" {
            let exchange: Exchange = serde_json::from_value(request.params).map_err(|_| ())?;
            serde_json::to_value(
                pty.as_mut()
                    .ok_or(())?
                    .exchange(&exchange)
                    .map_err(|_| ())?,
            )
            .map_err(|_| ())?
        } else if request.method == "target.pty.close" {
            let close: Close = serde_json::from_value(request.params).map_err(|_| ())?;
            let reply = pty
                .as_mut()
                .ok_or(())?
                .close(close.cancel)
                .map_err(|_| ())?;
            pty.take();
            serde_json::to_value(reply).map_err(|_| ())?
        } else if request.method == "target.completed" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Completed {
                grant: String,
                evidence: Value,
            }
            let completed: Completed = serde_json::from_value(request.params).map_err(|_| ())?;
            if !completed.evidence.is_object() || !pending.remove(&completed.grant) {
                return Err(());
            }
            json!({"received":completed.grant})
        } else {
            let operation = request.method.strip_prefix("target.").ok_or(())?;
            if !fabric_runner::target::OPERATIONS.contains(&operation)
                && ![
                    "environment.ensure",
                    "environment.inspect",
                    "environment.destroy",
                    "environment.workspace",
                ]
                .contains(&operation)
            {
                return Err(());
            }
            let job: Job = if operation.starts_with("environment.")
                && operation != "environment.workspace"
            {
                let env: EnvironmentJob = serde_json::from_value(request.params).map_err(|_| ())?;
                Job {
                    placement: env.placement,
                    run_id: env.environment_id.to_string(),
                    grant: env.grant,
                    actions: env.actions,
                    context: env.context,
                }
            } else {
                serde_json::from_value(request.params).map_err(|_| ())?
            };
            if job.placement != "local"
                || job.run_id.is_empty()
                || job.run_id.len() > 128
                || job.grant.is_empty()
                || job.grant.len() > 128
                || !job
                    .grant
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                || !seen.insert(job.grant.clone())
            {
                return Err(());
            }
            let action = match operation {
                "prepare" | "environment.workspace" => Action::Fork {
                    strategy: ForkMode::CloneOrCopy,
                },
                "start" => Action::Spawn,
                "inspect" => Action::Inspect,
                "cancel" => Action::Terminate,
                "collect" if job.actions.contains(&Action::Capture) => Action::Capture,
                "collect" => Action::Recover,
                "reclaim" => Action::Reclaim,
                "environment.ensure" => Action::EnsureEnvironment,
                "environment.inspect" => Action::InspectEnvironment,
                "environment.destroy" => Action::DestroyEnvironment,
                _ => return Err(()),
            };
            if !job.context.is_object()
                || !job.actions.contains(&action)
                || !pending.insert(job.grant.clone())
            {
                return Err(());
            }
            json!({"grant":job.grant,"action":action})
        };
        let mut response = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result}))
            .map_err(|_| ())?;
        response.push(b'\n');
        if response.len() > 65_536 {
            return Err(());
        }
        let mut written = 0;
        while written < response.len() {
            if cancellation.is_cancelled() {
                return Err(());
            }
            if let Some(session) = pty.as_mut() {
                session.tick().map_err(|_| ())?;
            }
            match output.write(&response[written..]) {
                Ok(0) => return Err(()),
                Ok(count) => written += count,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted =>
                {
                    let mut fd = libc::pollfd {
                        fd: output.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe {
                        libc::poll(&mut fd, 1, POLL_MS);
                    }
                    if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        return Err(());
                    }
                }
                Err(_) => return Err(()),
            }
        }
        if shutdown {
            return Ok(());
        }
        id = id.checked_add(1).ok_or(())?;
    }
}
fn main() -> std::process::ExitCode {
    if serve().is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("Fabric local target: invalid control exchange.");
        std::process::ExitCode::FAILURE
    }
}
