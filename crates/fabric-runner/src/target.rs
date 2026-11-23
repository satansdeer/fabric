//! Installed target drivers propose operations; only the host executes authority-bearing primitives.
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use fabric_domain::{ErrorKind, FabricError, HarnessId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::CancellationToken;
use crate::plugin::{PluginKind, PluginManifest, PluginSession, configured_registry, failure};

pub const OPERATIONS: [&str; 6] = ["prepare", "start", "inspect", "cancel", "collect", "reclaim"];

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPin {
    pub manifest: PluginManifest,
    pub executable_sha256: String,
    pub placement: String,
    pub registry: Option<PathBuf>,
}

fn executable_hash(path: &Path) -> Result<String, FabricError> {
    let mut file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
        .map_err(|_| failure("target_unavailable"))?;
    let metadata = file.metadata().map_err(|_| failure("target_unavailable"))?;
    // Includes installed language runtimes, not just the separately bounded plugin package.
    const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
    if !metadata.is_file() || metadata.len() > MAX_EXECUTABLE_BYTES
        || (metadata.uid() != unsafe { libc::geteuid() } && metadata.uid() != 0)
        || metadata.mode() & 0o022 != 0 {
        return Err(failure("unsafe_target_executable"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 16384];
    let mut remaining = MAX_EXECUTABLE_BYTES + 1;
    loop {
        let count = (&mut file).take(remaining).read(&mut buffer)
            .map_err(|_| failure("target_unavailable"))?;
        if count == 0 { break; }
        remaining -= count as u64;
        if remaining == 0 { return Err(failure("unsafe_target_executable")); }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl TargetPin {
    pub fn select(placement: &str) -> Result<Self, FabricError> {
        let id = HarnessId::try_from(placement.to_owned())?;
        let registry = configured_registry()?;
        let manifest = if let Some(registry) = registry.as_ref()
            .filter(|registry| registry.plugins.iter().any(|p| p.manifest.id == id)) {
            registry.select(&id, PluginKind::Target)?
        } else if placement == "local" {
            let executable = std::env::current_exe().map_err(|_| failure("target_unavailable"))?
                .parent().ok_or_else(|| failure("target_unavailable"))?.join("fabric-local-target");
            serde_json::from_value(json!({"id":"local","version":"0.1.0","kind":"target",
                "protocol":{"min":1,"max":1},"executable":executable,"args":[],
                "capabilities":["target.execute"]})).map_err(|_| failure("invalid_target"))?
        } else {
            return Err(FabricError { kind: ErrorKind::UnsupportedPlacement,
                message: "No installed target accepts the requested placement.".into() });
        };
        let mut pin = Self::from_manifest(manifest, placement)?;
        if registry.as_ref().is_some_and(|r| r.plugins.iter().any(|p| p.manifest.id == id)) {
            pin.registry = std::env::var_os("FABRIC_PLUGIN_REGISTRY").map(PathBuf::from);
        }
        Ok(pin)
    }

    pub fn from_manifest(manifest: PluginManifest, placement: &str) -> Result<Self, FabricError> {
        manifest.validate()?;
        manifest.verify_integrity()?;
        let executable_sha256 = executable_hash(&manifest.executable)?;
        Ok(Self { manifest, executable_sha256, placement: placement.into(), registry: None })
    }

    pub fn open(&self, cancellation: &CancellationToken) -> Result<Target, FabricError> {
        self.open_with_timeout(cancellation, Duration::from_secs(5))
    }

    pub fn preflight(&self, cancellation: &CancellationToken) -> Result<(), FabricError> {
        let target = self.open(cancellation)?;
        target.operation("inspect", "preflight")?;
        target.finish()
    }

    pub(crate) fn open_with_timeout(&self, cancellation: &CancellationToken, timeout: Duration) -> Result<Target, FabricError> {
        if self.manifest.kind != PluginKind::Target || self.manifest.capabilities != ["target.execute"]
            || self.manifest.id.as_str() != self.placement
            || executable_hash(&self.manifest.executable)? != self.executable_sha256 {
            return Err(failure("target_pin_mismatch"));
        }
        // Revocation remains effective for accepted work and recovery. Replacement never silently repins.
        let registry = match &self.registry {
            Some(path) => Some(crate::plugin::PluginRegistry::load(path)?),
            None => configured_registry()?,
        };
        if let Some(registry) = registry {
            if let Some(installed) = registry.plugins.iter().find(|p| p.manifest.id == self.manifest.id) {
                if installed.revoked { return Err(failure("revoked")); }
                if serde_json::to_value(&installed.manifest).ok() != serde_json::to_value(&self.manifest).ok() {
                    return Err(failure("target_pin_mismatch"));
                }
            } else if self.registry.is_some() || self.manifest.integrity.is_some() {
                return Err(failure("target_unavailable"));
            }
        }
        if cancellation.is_cancelled() { return Err(failure("cancelled")); }
        // A cancelled worker must still be able to request target cancellation and cleanup.
        let mut session = PluginSession::open(&self.manifest, timeout, cancellation)?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Capabilities { placements: Vec<String>, operations: Vec<String>, optional: Vec<String> }
        let caps: Capabilities = serde_json::from_value(session.call("target.capabilities", json!({}))?)
            .map_err(|_| failure("invalid_target_capabilities"))?;
        if caps.placements != [self.placement.as_str()] || caps.operations != OPERATIONS || !caps.optional.is_empty() {
            return Err(failure("unsupported_target_capabilities"));
        }
        session.bound_target_operations();
        Ok(Target { pin: self.clone(), session: Mutex::new(Some(session)) })
    }
}

pub struct Target {
    pin: TargetPin,
    session: Mutex<Option<PluginSession>>,
}
impl Target {
    /// This is an executable proposal, not readiness or completion evidence. Unknown operations,
    /// paths, process IDs, exit statuses, and remote-to-local proposals cannot cross this boundary.
    pub fn operation(&self, operation: &str, run: &str) -> Result<(), FabricError> {
        if !OPERATIONS.contains(&operation) { return Err(failure("unsupported_target_operation")); }
        let mut session = self.session.lock().map_err(|_| failure("target_session_failed"))?;
        let session = session.as_mut().ok_or_else(|| failure("target_session_failed"))?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Proposal { operation: String }
        let proposal: Proposal = serde_json::from_value(session.call(&format!("target.{operation}"),
            json!({"placement":self.pin.placement,"run_id":run}))?)
            .map_err(|_| failure("invalid_target_proposal"))?;
        if self.pin.placement != "local" || proposal.operation != format!("local.{operation}") {
            return Err(FabricError { kind: ErrorKind::UnsupportedPlacement,
                message: "Target operation is not supported for this placement; no local fallback.".into() });
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<(), FabricError> {
        let session = self.session.lock().map_err(|_| failure("target_session_failed"))?
            .take().ok_or_else(|| failure("target_session_failed"))?;
        session.finish()
    }

    pub fn execute(
        &self, run: &str, spec: &crate::CommandSpec, cwd: &Path,
        capture: crate::capture::Capture<'_>, limits: &crate::ExecutionLimits,
        cancellation: &CancellationToken, lease: Option<&File>,
        validate_launch: impl FnOnce() -> Result<(), FabricError>,
        started: impl FnOnce(u32) -> Result<(), FabricError>,
    ) -> Result<crate::Execution, FabricError> {
        self.operation("start", run)?;
        validate_launch()?;
        crate::execute_observed(spec, cwd, capture, limits, cancellation, lease, started, |stopping| {
            self.operation(if stopping { "cancel" } else { "inspect" }, run)
        })
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Ok(session) = self.session.get_mut() {
            if let Some(session) = session.take() { let _ = session.finish(); }
        }
    }
}

