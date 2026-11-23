use super::*;
use std::process::{ChildStdin, ChildStdout};

/// One bounded control process; native normalization reuses it for the stream.
pub(crate) struct PluginSession {
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    group: crate::supervision::ProcessGroup,
    _home: tempfile::TempDir,
    next_id: u64,
    timeout: Duration,
    cancel: CancellationToken,
    start: Instant,
    frame_limit: usize,
    persistent: bool,
}
impl PluginSession {
    pub(crate) fn open(
        manifest: &PluginManifest,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self, FabricError> {
        manifest.validate()?;
        if !manifest.compatible() {
            return Err(failure("unsupported_protocol"));
        }
        manifest.verify_integrity()?;
        if cancel.is_cancelled() {
            return Err(failure("cancelled"));
        }
        if timeout.is_zero() || timeout > Duration::from_secs(60) {
            return Err(failure("invalid_timeout"));
        }
        let start = Instant::now();
        let mut command = Command::new(&manifest.executable);
        command
            .args(&manifest.args)
            .arg("--fabric-plugin-stdio")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let home = crate::profile::private_environment(&mut command)
            .map_err(|_| failure("private_environment"))?;
        let child = command.spawn().map_err(|_| failure("spawn_failed"))?;
        let mut group = crate::supervision::ProcessGroup::new(child, start);
        let input = group
            .child
            .stdin
            .take()
            .ok_or_else(|| failure("pipe_failed"))?;
        let output = group
            .child
            .stdout
            .take()
            .ok_or_else(|| failure("pipe_failed"))?;
        for fd in [input.as_raw_fd(), output.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(failure("pipe_failed"));
            }
        }
        let mut session = Self {
            input: Some(input),
            output: BufReader::new(output),
            group,
            _home: home,
            next_id: 1,
            timeout,
            cancel: cancel.clone(),
            start,
            frame_limit: if manifest.native.is_some() {
                MAX_FRAME * 2
            } else {
                MAX_FRAME
            },
            persistent: manifest.native.is_some() || manifest.kind == PluginKind::Target,
        };
        let hello: Hello =
            serde_json::from_value(session.call("initialize", json!({"protocol":1}))?)
                .map_err(|_| failure("invalid_hello"))?;
        if hello.protocol != 1
            || hello.id != manifest.id
            || hello.version != manifest.version
            || hello.kind != manifest.kind
        {
            return Err(failure("negotiation_failed"));
        }
        let health: Ready = serde_json::from_value(session.call("health", json!({}))?)
            .map_err(|_| failure("invalid_health"))?;
        if !health.ready {
            return Err(failure("unhealthy"));
        }
        Ok(session)
    }
    pub(crate) fn call(&mut self, method: &str, params: Value) -> Result<Value, FabricError> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| failure("request_id_exhausted"))?;
        let deadline = if self.persistent {
            Instant::now() + self.timeout
        } else {
            self.start + self.timeout
        };
        exchange_bounded(
            (
                self.input.as_mut().ok_or_else(|| failure("closed_input"))?,
                &mut self.output,
            ),
            id,
            method,
            params,
            deadline,
            &self.cancel,
            self.frame_limit,
        )
    }
    pub(crate) fn set_cancellation(&mut self, cancellation: &CancellationToken) {
        self.cancel = cancellation.clone();
    }
    pub(crate) fn bound_target_operations(&mut self) {
        self.cancel = CancellationToken::default();
        self.timeout = self.timeout.min(Duration::from_secs(1));
    }
    pub(crate) fn finish(mut self) -> Result<(), FabricError> {
        let ready: Ready = serde_json::from_value(self.call("shutdown", json!({}))?)
            .map_err(|_| failure("invalid_shutdown"))?;
        if !ready.ready {
            return Err(failure("invalid_shutdown"));
        }
        self.input.take();
        let limits = crate::ExecutionLimits {
            timeout: if self.persistent {
                self.start.elapsed() + self.timeout
            } else {
                self.timeout
            },
            term_grace: Duration::from_millis(10),
        };
        let (status, reason) = self
            .group
            .supervise(&limits, &self.cancel, || {})
            .map_err(|_| failure("shutdown_failed"))?;
        if reason.is_some() || !status.success() {
            return Err(failure("shutdown_failed"));
        }
        Ok(())
    }
}
