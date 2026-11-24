//! Installed targets negotiate either Core-owned host primitives or target-owned remote resources.
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fabric_domain::{ErrorKind, FabricError, HarnessId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::CancellationToken;
use crate::plugin::{PluginKind, PluginManifest, PluginSession, configured_registry, failure};

pub const OPERATIONS: [&str; 6] = [
    "prepare", "start", "inspect", "cancel", "collect", "reclaim",
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPin {
    pub manifest: PluginManifest,
    pub executable_sha256: String,
    pub placement: String,
    pub registry: Option<PathBuf>,
}

fn executable_hash(path: &Path) -> Result<String, FabricError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| failure("target_unavailable"))?;
    let metadata = file.metadata().map_err(|_| failure("target_unavailable"))?;
    // Includes installed language runtimes, not just the separately bounded plugin package.
    const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
    if !metadata.is_file()
        || metadata.len() > MAX_EXECUTABLE_BYTES
        || (metadata.uid() != unsafe { libc::geteuid() } && metadata.uid() != 0)
        || metadata.mode() & 0o022 != 0
    {
        return Err(failure("unsafe_target_executable"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 16384];
    let mut remaining = MAX_EXECUTABLE_BYTES + 1;
    loop {
        let count = (&mut file)
            .take(remaining)
            .read(&mut buffer)
            .map_err(|_| failure("target_unavailable"))?;
        if count == 0 {
            break;
        }
        remaining -= count as u64;
        if remaining == 0 {
            return Err(failure("unsafe_target_executable"));
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl TargetPin {
    pub fn select(placement: &str) -> Result<Self, FabricError> {
        let id = HarnessId::try_from(placement.to_owned())?;
        let registry = configured_registry()?;
        let manifest = if let Some(registry) = registry
            .as_ref()
            .filter(|registry| registry.plugins.iter().any(|p| p.manifest.id == id))
        {
            registry.select(&id, PluginKind::Target)?
        } else if placement == "local" {
            let executable = std::env::current_exe()
                .map_err(|_| failure("target_unavailable"))?
                .parent()
                .ok_or_else(|| failure("target_unavailable"))?
                .join("fabric-local-target");
            serde_json::from_value(json!({"id":"local","version":"0.1.0","kind":"target",
                "protocol":{"min":1,"max":1},"executable":executable,"args":[],
                "capabilities":["target.execute"]}))
            .map_err(|_| failure("invalid_target"))?
        } else {
            return Err(FabricError {
                kind: ErrorKind::UnsupportedPlacement,
                message: "No installed target accepts the requested placement.".into(),
            });
        };
        let mut pin = Self::from_manifest(manifest, placement)?;
        if registry
            .as_ref()
            .is_some_and(|r| r.plugins.iter().any(|p| p.manifest.id == id))
        {
            pin.registry = std::env::var_os("FABRIC_PLUGIN_REGISTRY").map(PathBuf::from);
        }
        Ok(pin)
    }

    pub fn from_manifest(manifest: PluginManifest, placement: &str) -> Result<Self, FabricError> {
        manifest.validate()?;
        manifest.verify_integrity()?;
        let executable_sha256 = executable_hash(&manifest.executable)?;
        Ok(Self {
            manifest,
            executable_sha256,
            placement: placement.into(),
            registry: None,
        })
    }

    pub fn open(&self, cancellation: &CancellationToken) -> Result<Target, FabricError> {
        self.open_with_timeout(cancellation, Duration::from_secs(5))
    }

    pub fn preflight(&self, cancellation: &CancellationToken) -> Result<(), FabricError> {
        self.open(cancellation)?.finish()
    }

    pub(crate) fn open_with_timeout(
        &self,
        cancellation: &CancellationToken,
        timeout: Duration,
    ) -> Result<Target, FabricError> {
        if self.manifest.kind != PluginKind::Target
            || self.manifest.capabilities != ["target.execute"]
            || self.manifest.id.as_str() != self.placement
            || executable_hash(&self.manifest.executable)? != self.executable_sha256
        {
            return Err(failure("target_pin_mismatch"));
        }
        // Revocation remains effective for accepted work and recovery. Replacement never silently repins.
        let registry = match &self.registry {
            Some(path) => Some(crate::plugin::PluginRegistry::load(path)?),
            None => configured_registry()?,
        };
        if let Some(registry) = registry {
            if let Some(installed) = registry
                .plugins
                .iter()
                .find(|p| p.manifest.id == self.manifest.id)
            {
                if installed.revoked {
                    return Err(failure("revoked"));
                }
                if serde_json::to_value(&installed.manifest).ok()
                    != serde_json::to_value(&self.manifest).ok()
                {
                    return Err(failure("target_pin_mismatch"));
                }
            } else if self.registry.is_some() || self.manifest.integrity.is_some() {
                return Err(failure("target_unavailable"));
            }
        }
        if cancellation.is_cancelled() {
            return Err(failure("cancelled"));
        }
        // A cancelled worker must still be able to request target cancellation and cleanup.
        let mut session = PluginSession::open(&self.manifest, timeout, cancellation)?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Capabilities {
            placements: Vec<String>,
            operations: Vec<String>,
            optional: Vec<String>,
        }
        let caps: Capabilities =
            serde_json::from_value(session.call("target.capabilities", json!({}))?)
                .map_err(|_| failure("invalid_target_capabilities"))?;
        if caps.placements != [self.placement.as_str()]
            || caps.operations != OPERATIONS
            || !(caps.optional.is_empty()
                || caps.optional == ["environment.v1"]
                || caps.optional == ["environment.v1", "pty.v1"]
                || caps.optional == ["environment.v1", "pty.v1", "remote.v1"])
        {
            return Err(failure("unsupported_target_capabilities"));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Placement {
            backend: String,
        }
        let remote = caps.optional == ["environment.v1", "pty.v1", "remote.v1"];
        let placement: Placement = serde_json::from_value(session.call(
            "target.placement",
            json!({"placement":self.placement,"backends":if remote {vec!["remote.v1"]} else {vec!["host.local"]}}),
        )?)
        .map_err(|_| failure("invalid_target_placement"))?;
        if placement.backend != if remote { "remote.v1" } else { "host.local" } {
            return Err(FabricError {
                kind: ErrorKind::UnsupportedPlacement,
                message: "Target refused available placement; no execution fallback.".into(),
            });
        }
        let pty = remote || caps.optional == ["environment.v1", "pty.v1"];
        let environment = if !caps.optional.is_empty() {
            let capabilities: fabric_domain::EnvironmentCapabilities = serde_json::from_value(
                session.call("target.environment.capabilities", json!({"version":1}))?,
            )
            .map_err(|_| failure("invalid_environment_capabilities"))?;
            let mut expected = fabric_domain::EnvironmentCapabilities::local();
            expected.interactive_exec_pty = pty;
            expected.infrastructure_diagnostics = remote;
            if capabilities != expected {
                return Err(unsupported_environment(
                    "unsupported environment capability negotiation",
                ));
            }
            Some(capabilities)
        } else {
            None
        };
        session.bound_target_operations();
        Ok(Target {
            pin: self.clone(),
            session: Mutex::new(Some(session)),
            next_grant: AtomicU64::new(1),
            environment,
            remote,
        })
    }
}

/// Only these scoped primitives can be offered by an authority-bearing caller.
/// The target chooses a plan; the host executes it against captured validated objects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "primitive", deny_unknown_fields)]
pub enum Action {
    #[serde(rename = "workspace.fork")]
    Fork { strategy: ForkMode },
    #[serde(rename = "process.spawn")]
    Spawn,
    #[serde(rename = "process.inspect")]
    Inspect,
    #[serde(rename = "process.terminate")]
    Terminate,
    #[serde(rename = "artifacts.capture")]
    Capture,
    #[serde(rename = "artifacts.recover")]
    Recover,
    #[serde(rename = "workspace.reclaim")]
    Reclaim,
    #[serde(rename = "environment.ensure")]
    EnsureEnvironment,
    #[serde(rename = "environment.inspect")]
    InspectEnvironment,
    #[serde(rename = "environment.destroy")]
    DestroyEnvironment,
    #[serde(rename = "unsupported")]
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkMode {
    CloneOrCopy,
    Copy,
}

pub struct Target {
    pin: TargetPin,
    session: Mutex<Option<PluginSession>>,
    next_grant: AtomicU64,
    environment: Option<fabric_domain::EnvironmentCapabilities>,
    remote: bool,
}
/// A host effect committed even if its target receipt failed. Callers must account
/// for `value` before surfacing `failure`; an error here is never a successful receipt.
pub struct Committed<T> {
    pub value: T,
    pub failure: Option<FabricError>,
}

impl<T> Committed<T> {
    pub fn into_result(self) -> Result<T, FabricError> {
        match self.failure {
            Some(failure) => Err(failure),
            None => Ok(self.value),
        }
    }
}

fn remote_protocol_error(mut error: FabricError) -> FabricError {
    error.kind = ErrorKind::TargetProtocol;
    error
}
fn remote_failure(code: &'static str) -> FabricError {
    remote_protocol_error(failure(code))
}

impl Target {
    /// Remote effects are owned by the explicitly negotiated target, never host grants.
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// One invocation, polled without retrying it. Every wire request retains the
    /// existing target deadline; healthy remote work has no implicit wall-clock stop.
    pub fn remote_call(&self, operation: &str, input: Value) -> Result<Value, FabricError> {
        if !self.remote {
            return Err(remote_failure("remote_backend_not_negotiated"));
        }
        let reply = self
            .call(
                "target.remote.invoke",
                json!({"version":1,"operation":operation,"input":input}),
            )
            .map_err(remote_protocol_error)?;
        let ticket = reply
            .get("ticket")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(|| remote_failure("invalid_remote_ticket"))?;
        loop {
            let reply = self
                .call("target.remote.poll", json!({"ticket":ticket}))
                .map_err(remote_protocol_error)?;
            match reply.get("state").and_then(Value::as_str) {
                Some("pending") if reply.as_object().is_some_and(|v| v.len() == 1) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Some("complete") if reply.as_object().is_some_and(|v| v.len() == 2) => {
                    return reply
                        .get("result")
                        .cloned()
                        .ok_or_else(|| remote_failure("invalid_remote_result"));
                }
                Some("failed") if reply["scope"] == "harness" => {
                    let kind = serde_json::from_value(reply["kind"].clone())
                        .map_err(|_| remote_failure("invalid_remote_error"))?;
                    return Err(FabricError {
                        kind,
                        message: "Remote harness operation failed.".into(),
                    });
                }
                Some("failed") => return Err(remote_failure("remote_target_failed")),
                _ => return Err(remote_failure("invalid_remote_result")),
            }
        }
    }
    pub fn environment_capabilities(
        &self,
    ) -> Result<&fabric_domain::EnvironmentCapabilities, FabricError> {
        self.environment.as_ref().ok_or_else(|| unsupported_environment(
            "Target did not negotiate environment.v1; persistent_home and interactive_exec_pty are unsupported",
        ))
    }

    pub fn require_environment(&self, pty: bool) -> Result<(), FabricError> {
        let capabilities = self.environment_capabilities()?;
        if !capabilities.persistent_home || (pty && !capabilities.interactive_exec_pty) {
            return Err(unsupported_environment(
                "Requested persistent_home/interactive_exec_pty is unsupported",
            ));
        }
        Ok(())
    }

    pub fn open_exec(&self, request: &crate::pty::OpenExec) -> Result<(), FabricError> {
        if self.remote {
            return Err(failure("host_pty_forbidden_for_remote_backend"));
        }
        self.require_environment(true)?;
        request.size.validate()?;
        request.limits.validate()?;
        if request.version != 1 || request.placement != self.pin.placement {
            return Err(crate::pty::error());
        }
        let reply: crate::pty::Opened = serde_json::from_value(self.call(
            "target.pty.openExec",
            serde_json::to_value(request).map_err(|_| crate::pty::error())?,
        )?)
        .map_err(|_| crate::pty::error())?;
        if !reply.opened {
            return Err(crate::pty::error());
        }
        Ok(())
    }

    pub fn exchange_exec(
        &self,
        request: &crate::pty::Exchange,
    ) -> Result<crate::pty::Reply, FabricError> {
        self.require_environment(true)?;
        request.validate()?;
        let params = serde_json::to_value(request).map_err(|_| crate::pty::error())?;
        let value = if self.remote {
            self.remote_call("pty.exchange", params)?
        } else {
            self.call("target.pty.exchange", params)?
        };
        let reply: crate::pty::Reply =
            serde_json::from_value(value).map_err(|_| crate::pty::error())?;
        reply.validate(request.input.len())?;
        Ok(reply)
    }

    pub fn close_exec(&self, cancel: bool) -> Result<crate::pty::Reply, FabricError> {
        let value = if self.remote {
            self.remote_call("pty.close", json!({"cancel":cancel}))?
        } else {
            self.call("target.pty.close", json!({"cancel":cancel}))?
        };
        let reply: crate::pty::Reply =
            serde_json::from_value(value).map_err(|_| crate::pty::error())?;
        reply.validate(0)?;
        if reply.closed.is_none() || !reply.output.is_empty() {
            return Err(crate::pty::error());
        }
        Ok(reply)
    }

    /// A grant exists for this call only. No plugin paths, PIDs, argv, outcome or
    /// deletion claims are accepted. Only the supplied executor can produce evidence.
    pub fn drive<T>(
        &self,
        operation: &str,
        run: &str,
        actions: &[Action],
        context: serde_json::Value,
        execute: impl FnOnce(Action) -> Result<(T, serde_json::Value), FabricError>,
    ) -> Result<Committed<T>, FabricError> {
        if self.remote {
            return Err(failure("host_grants_forbidden_for_remote_backend"));
        }
        let environment_operation = operation.strip_prefix("environment.");
        if let Some(operation) = environment_operation {
            let caps = self.environment_capabilities()?;
            if !caps
                .operations
                .iter()
                .any(|supported| supported == operation)
            {
                return Err(unsupported_environment("Unsupported environment operation"));
            }
        } else if !OPERATIONS.contains(&operation) {
            return Err(failure("unsupported_target_operation"));
        }
        if actions.is_empty() {
            return Err(failure("unsupported_target_operation"));
        }
        let grant = self.next_grant.fetch_add(1, Ordering::Relaxed).to_string();
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Plan {
            grant: String,
            action: Action,
        }
        let params = if environment_operation.is_some() && operation != "environment.workspace" {
            json!({"placement":self.pin.placement,"environment_id":run,"grant":grant,
                "actions":actions,"context":context})
        } else {
            json!({"placement":self.pin.placement,"run_id":run,"grant":grant,
                "actions":actions,"context":context})
        };
        let plan: Plan = serde_json::from_value(self.call(&format!("target.{operation}"), params)?)
            .map_err(|_| failure("invalid_target_plan"))?;
        if plan.action == Action::Unsupported {
            return Err(FabricError {
                kind: if environment_operation.is_some() {
                    ErrorKind::UnsupportedCapability
                } else {
                    ErrorKind::UnsupportedPlacement
                },
                message: "Target refused the granted operation; no execution fallback.".into(),
            });
        }
        if plan.grant != grant || !actions.contains(&plan.action) {
            return Err(failure("target_grant_mismatch"));
        }
        let (value, evidence) = execute(plan.action)?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Received {
            received: String,
        }
        let receipt = (|| {
            let received: Received = serde_json::from_value(self.call(
                "target.completed",
                json!({"grant":grant,"evidence":evidence}),
            )?)
            .map_err(|_| failure("invalid_target_receipt"))?;
            if received.received != grant {
                return Err(failure("target_receipt_mismatch"));
            }
            Ok(())
        })();
        Ok(Committed {
            value,
            failure: receipt.err(),
        })
    }

    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, FabricError> {
        self.session
            .lock()
            .map_err(|_| failure("target_session_failed"))?
            .as_mut()
            .ok_or_else(|| failure("target_session_failed"))?
            .call(method, params)
    }

    pub fn finish(&self) -> Result<(), FabricError> {
        let session = self
            .session
            .lock()
            .map_err(|_| failure("target_session_failed"))?
            .take()
            .ok_or_else(|| failure("target_session_failed"))?;
        session.finish()
    }

    pub fn execute(
        &self,
        run: &str,
        request: crate::ExecutionRequest<'_>,
        mut validate_launch: impl FnMut() -> Result<(), FabricError>,
        started: impl FnOnce(u32) -> Result<(), FabricError>,
    ) -> Result<crate::Execution, FabricError> {
        let cancellation = request.cancellation;
        let term_grace = request.limits.term_grace;
        let validate = |validate_launch: &mut dyn FnMut() -> Result<(), FabricError>| {
            if cancellation.is_cancelled() {
                return Err(failure("cancelled"));
            }
            validate_launch()
        };
        // No concrete start request reaches installed code before fresh authority checks.
        validate(&mut validate_launch)?;
        let mut target_failed = false;
        let committed = self.drive("start", run, &[Action::Spawn], json!({}), |_| {
            validate(&mut validate_launch)?;
            let execution = crate::execute_observed(request, started, |group, stopping| {
                let mut host_failed = false;
                let result = if stopping {
                    self.drive("cancel", run, &[Action::Terminate], json!({}), |_| {
                        let result = group
                            .signal(libc::SIGTERM, std::time::Instant::now() + term_grace)
                            .map_err(crate::execution_error);
                        host_failed = result.is_err();
                        result?;
                        Ok(((), json!({"termination_requested":true})))
                    })
                } else {
                    self.drive("inspect", run, &[Action::Inspect], json!({}), |_| {
                        let result = group.exited().map_err(crate::execution_error);
                        host_failed = result.is_err();
                        let exited = result?;
                        Ok(((), json!({"exited":exited})))
                    })
                };
                target_failed |= match &result {
                    Ok(committed) => committed.failure.is_some(),
                    Err(_) => !host_failed,
                };
                result?.into_result()
            })?;
            use std::os::unix::process::ExitStatusExt;
            let evidence = json!({"exit_code":execution.status.code(),"exit_signal":execution.status.signal(),
                "stop_reason":execution.stop_reason.map(|reason| reason.as_str())});
            Ok((execution, evidence))
        })?;
        let mut execution = committed.value;
        execution.target_failed = target_failed || committed.failure.is_some();
        execution.failure = execution.failure.or(committed.failure);
        Ok(execution)
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Ok(session) = self.session.get_mut() {
            if let Some(session) = session.take() {
                let _ = session.finish();
            }
        }
    }
}

fn unsupported_environment(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::UnsupportedCapability,
        message: message.into(),
    }
}
