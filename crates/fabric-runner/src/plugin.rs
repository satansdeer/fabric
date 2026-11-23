//! Owner-installed, trusted-local executables. This is a protocol boundary, not a sandbox.
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{AgentSpec, CancellationToken, CommandSpec, HarnessAdapter, HarnessInfo};
use fabric_domain::{ErrorKind, FabricError, HarnessId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[path = "plugin_session.rs"]
mod session;
pub(crate) use session::PluginSession;

pub const MAX_FRAME: usize = 65536;
const MAX_REGISTRY: u64 = 1024 * 1024;
pub(crate) fn failure(code: &'static str) -> FabricError {
    FabricError {
        kind: ErrorKind::Output,
        message: format!("Plugin: {code}."),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Harness,
    Target,
    Connector,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

/// Optional content pin for SDK packages. Legacy executable manifests remain supported.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginIntegrity {
    pub root: PathBuf,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub id: HarnessId,
    pub version: String,
    pub protocol: ProtocolRange,
    pub kind: PluginKind,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<PluginIntegrity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<crate::native_plugin::NativePluginMetadata>,
}
impl PluginManifest {
    pub fn load(path: &Path) -> Result<Self, FabricError> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| failure("manifest_unreadable"))?;
        let meta = file
            .metadata()
            .map_err(|_| failure("manifest_unreadable"))?;
        if !meta.is_file() || meta.len() > MAX_FRAME as u64 {
            return Err(failure("invalid_manifest"));
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_FRAME as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| failure("manifest_unreadable"))?;
        if bytes.len() > MAX_FRAME {
            return Err(failure("invalid_manifest"));
        }
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|_| failure("invalid_manifest"))?;
        manifest.validate()?;
        Ok(manifest)
    }
    pub fn validate(&self) -> Result<(), FabricError> {
        if !valid_version(&self.version)
            || !self.executable.is_absolute()
            || self.args.len() > 32
            || self.args.iter().any(|s| s.len() > 4096 || s.contains('\0'))
            || self.protocol.min == 0
            || self.protocol.min > self.protocol.max
        {
            return Err(failure("invalid_manifest"));
        }
        if self.integrity.as_ref().is_some_and(|pin| {
            !pin.root.is_absolute()
                || pin.sha256.len() != 64
                || !pin
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(failure("invalid_integrity"));
        }
        if let Some(native) = &self.native {
            native.validate()?;
            if self.kind != PluginKind::Harness || self.integrity.is_none() {
                return Err(failure("native_plugin_requires_content_pin"));
            }
        }
        let allowed = match self.kind {
            PluginKind::Harness => "harness.run",
            PluginKind::Target => "target.execute",
            PluginKind::Connector => "connector.evidence",
        };
        if self.capabilities != [allowed] {
            return Err(failure("unsupported_capability"));
        }
        Ok(())
    }
    pub fn compatible(&self) -> bool {
        self.protocol.min <= 1 && self.protocol.max >= 1
    }
    pub fn verify_integrity(&self) -> Result<(), FabricError> {
        let Some(pin) = &self.integrity else {
            return Ok(());
        };
        let root = &pin.root;
        let meta = fs::symlink_metadata(root).map_err(|_| failure("integrity_mismatch"))?;
        if !meta.is_dir()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.permissions().mode() & 0o077 != 0
            || root
                .canonicalize()
                .map_err(|_| failure("integrity_mismatch"))?
                != *root
        {
            return Err(failure("integrity_mismatch"));
        }
        let mut files = Vec::new();
        let mut entries = 0;
        integrity_files(root, 0, &mut entries, &mut files)?;
        files.sort();
        let mut digest = Sha256::new();
        let mut total = 0usize;
        let mut buffer = [0u8; 8192];
        for path in files {
            let name = path
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .ok_or_else(|| failure("integrity_mismatch"))?;
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .map_err(|_| failure("integrity_mismatch"))?;
            if !file
                .metadata()
                .map_err(|_| failure("integrity_mismatch"))?
                .is_file()
            {
                return Err(failure("integrity_mismatch"));
            }
            let mut content = Sha256::new();
            loop {
                let n = file
                    .read(&mut buffer)
                    .map_err(|_| failure("integrity_mismatch"))?;
                if n == 0 {
                    break;
                }
                total += n;
                if total > 32 * 1024 * 1024 {
                    return Err(failure("integrity_mismatch"));
                }
                content.update(&buffer[..n]);
            }
            digest.update(name.as_bytes());
            digest.update(b"\0");
            digest.update(hex_digest(content.finalize().into()));
            digest.update(b"\n");
        }
        if pin.sha256.as_bytes() != hex_digest(digest.finalize().into()).as_slice() {
            return Err(failure("integrity_mismatch"));
        }
        Ok(())
    }
}

fn hex_digest(bytes: [u8; 32]) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = [0; 64];
    for (i, byte) in bytes.into_iter().enumerate() {
        hex[i * 2] = HEX[(byte >> 4) as usize];
        hex[i * 2 + 1] = HEX[(byte & 15) as usize];
    }
    hex
}

fn integrity_files(
    root: &Path,
    depth: usize,
    entries: &mut usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), FabricError> {
    if depth > 16 {
        return Err(failure("integrity_mismatch"));
    }
    for entry in fs::read_dir(root).map_err(|_| failure("integrity_mismatch"))? {
        let entry = entry.map_err(|_| failure("integrity_mismatch"))?;
        *entries += 1;
        if *entries > 512 {
            return Err(failure("integrity_mismatch"));
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| failure("integrity_mismatch"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_@.-".contains(&b))
        {
            return Err(failure("integrity_mismatch"));
        }
        let kind = entry
            .file_type()
            .map_err(|_| failure("integrity_mismatch"))?;
        if kind.is_dir() {
            integrity_files(&entry.path(), depth + 1, entries, files)?;
        } else if kind.is_file() && files.len() < 256 {
            files.push(entry.path());
        } else {
            return Err(failure("integrity_mismatch"));
        }
    }
    Ok(())
}
fn valid_version(version: &str) -> bool {
    if version.len() > 64 {
        return false;
    }
    let identifiers = |value: &str, numeric_leading_zero: bool| {
        value.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && (numeric_leading_zero
                    || part.len() == 1
                    || !part.starts_with('0')
                    || !part.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(v, b)| (v, Some(b)));
    if build.is_some_and(|build| !identifiers(build, true)) {
        return false;
    }
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(v, p)| (v, Some(p)));
    if pre.is_some_and(|pre| !identifiers(pre, false)) {
        return false;
    }
    let mut parts = core.split('.');
    (0..3).all(|_| {
        parts.next().is_some_and(|part| {
            !part.is_empty()
                && (part.len() == 1 || !part.starts_with('0'))
                && part.bytes().all(|b| b.is_ascii_digit())
        })
    }) && parts.next().is_none()
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub revoked: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRegistry {
    pub plugins: Vec<InstalledPlugin>,
}
#[derive(Serialize)]
pub struct Compatibility {
    pub id: HarnessId,
    pub compatible: bool,
    pub code: &'static str,
}
impl PluginRegistry {
    pub fn load(root: &Path) -> Result<Self, FabricError> {
        private_root(root)?;
        let path = root.join("plugins.json");
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err(failure("registry_unreadable")),
        };
        let meta = file
            .metadata()
            .map_err(|_| failure("registry_unreadable"))?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.permissions().mode() & 0o077 != 0
            || meta.len() > MAX_REGISTRY
        {
            return Err(failure("unsafe_registry"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_REGISTRY + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| failure("registry_unreadable"))?;
        if bytes.len() as u64 > MAX_REGISTRY {
            return Err(failure("registry_too_large"));
        }
        let registry: Self =
            serde_json::from_slice(&bytes).map_err(|_| failure("invalid_registry"))?;
        for (i, plugin) in registry.plugins.iter().enumerate() {
            plugin.manifest.validate()?;
            if registry.plugins[..i]
                .iter()
                .any(|p| p.manifest.id == plugin.manifest.id)
                || builtin(&plugin.manifest.id)
            {
                return Err(failure("duplicate_id"));
            }
        }
        Ok(registry)
    }
    pub fn save(&self, root: &Path) -> Result<(), FabricError> {
        private_root(root)?;
        let bytes = serde_json::to_vec(self).map_err(|_| failure("invalid_registry"))?;
        if bytes.len() as u64 > MAX_REGISTRY {
            return Err(failure("registry_too_large"));
        }
        let mut file =
            tempfile::NamedTempFile::new_in(root).map_err(|_| failure("registry_write"))?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| failure("registry_write"))?;
        file.write_all(&bytes)
            .and_then(|_| file.as_file().sync_all())
            .map_err(|_| failure("registry_write"))?;
        file.persist(root.join("plugins.json"))
            .map_err(|_| failure("registry_write"))?;
        Ok(())
    }
    pub fn install(&mut self, manifest: PluginManifest, upgrade: bool) -> Result<(), FabricError> {
        manifest.validate()?;
        manifest.verify_integrity()?;
        if builtin(&manifest.id) {
            return Err(failure("duplicate_id"));
        }
        if let Some(existing) = self
            .plugins
            .iter_mut()
            .find(|p| p.manifest.id == manifest.id)
        {
            if !upgrade {
                return Err(failure("duplicate_id"));
            }
            if existing.manifest.kind != manifest.kind {
                return Err(failure("kind_changed"));
            }
            *existing = InstalledPlugin {
                manifest,
                revoked: false,
            };
        } else {
            if upgrade {
                return Err(failure("missing_plugin"));
            }
            self.plugins.push(InstalledPlugin {
                manifest,
                revoked: false,
            });
        }
        Ok(())
    }
    pub fn revoke(&mut self, id: &HarnessId) -> Result<(), FabricError> {
        self.plugins
            .iter_mut()
            .find(|p| p.manifest.id == *id)
            .ok_or_else(|| failure("missing_plugin"))?
            .revoked = true;
        Ok(())
    }
    pub fn reports(&self) -> Vec<Compatibility> {
        self.plugins
            .iter()
            .map(|p| Compatibility {
                id: p.manifest.id.clone(),
                compatible: !p.revoked
                    && p.manifest.compatible()
                    && p.manifest.executable.is_file(),
                code: if p.revoked {
                    "revoked"
                } else if !p.manifest.compatible() {
                    "unsupported_protocol"
                } else if !p.manifest.executable.is_file() {
                    "missing_executable"
                } else {
                    "compatible"
                },
            })
            .collect()
    }
    pub fn select(&self, id: &HarnessId, kind: PluginKind) -> Result<PluginManifest, FabricError> {
        let p = self
            .plugins
            .iter()
            .find(|p| p.manifest.id == *id)
            .ok_or_else(|| failure("missing_plugin"))?;
        if p.revoked {
            return Err(failure("revoked"));
        }
        if !p.manifest.compatible() {
            return Err(failure("unsupported_protocol"));
        }
        if p.manifest.kind != kind {
            return Err(failure("wrong_kind"));
        }
        Ok(p.manifest.clone())
    }
}
fn builtin(id: &HarnessId) -> bool {
    matches!(id.as_str(), "fake" | "fake-a" | "fake-b")
}
fn private_root(root: &Path) -> Result<(), FabricError> {
    if !root.is_absolute() {
        return Err(failure("registry_must_be_absolute"));
    }
    let meta = fs::symlink_metadata(root).map_err(|_| failure("registry_unreadable"))?;
    if !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o077 != 0
    {
        return Err(failure("unsafe_registry"));
    }
    let canonical = root
        .canonicalize()
        .map_err(|_| failure("registry_unreadable"))?;
    if canonical != root {
        return Err(failure("registry_must_be_canonical"));
    }
    // An explicit registry is still forbidden anywhere inside a checkout.
    if canonical.ancestors().any(|p| p.join(".git").exists()) {
        return Err(failure("project_registry_forbidden"));
    }
    Ok(())
}
pub fn configured_registry() -> Result<Option<PluginRegistry>, FabricError> {
    std::env::var_os("FABRIC_PLUGIN_REGISTRY")
        .map(|p| PluginRegistry::load(Path::new(&p)))
        .transpose()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorEvidence {
    pub available: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    ready: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    protocol: u32,
    id: HarnessId,
    version: String,
    kind: PluginKind,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    jsonrpc: String,
    id: u64,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcError {
    code: i64,
    message: String,
}

/// Every call owns and reaps its process group, including protocol failure paths.
pub struct PluginHost {
    pub timeout: Duration,
}
impl Default for PluginHost {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
        }
    }
}
impl PluginHost {
    pub fn target(
        &self,
        manifest: &PluginManifest,
        cancel: &CancellationToken,
    ) -> Result<crate::target::Target, FabricError> {
        crate::target::TargetPin::from_manifest(manifest.clone(), manifest.id.as_str())?
            .open_with_timeout(cancel, self.timeout)
    }
    pub(crate) fn invoke<T: serde::de::DeserializeOwned>(
        &self,
        manifest: &PluginManifest,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<T, FabricError> {
        let mut session = PluginSession::open(manifest, self.timeout, cancel)?;
        let result = serde_json::from_value(session.call(method, params)?)
            .map_err(|_| failure("invalid_result"))?;
        session.finish()?;
        Ok(result)
    }
    pub fn harness_info(
        &self,
        manifest: &PluginManifest,
        cancel: &CancellationToken,
    ) -> Result<HarnessInfo, FabricError> {
        if manifest.kind != PluginKind::Harness {
            return Err(failure("wrong_kind"));
        }
        if manifest.native.is_some() {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Probed {
                version: String,
            }
            let reply: crate::native_plugin::NativeReply<Probed> = self.invoke(
                manifest,
                "harness.probe",
                json!({"context":crate::native_plugin::context(manifest, None, false)?}),
                cancel,
            )?;
            let info = crate::native_plugin::resolve_reply(reply)?;
            fabric_domain::HarnessVersion::validate(&info.version)?;
            return Ok(HarnessInfo {
                version: info.version,
            });
        }
        let ready: Ready = self.invoke(manifest, "harness.plan", json!({}), cancel)?;
        if !ready.ready {
            return Err(failure("harness_not_ready"));
        }
        Ok(HarnessInfo {
            version: manifest.version.clone(),
        })
    }
    pub fn connector_evidence(
        &self,
        manifest: &PluginManifest,
        cancel: &CancellationToken,
    ) -> Result<ConnectorEvidence, FabricError> {
        if manifest.kind != PluginKind::Connector {
            return Err(failure("wrong_kind"));
        }
        self.invoke(manifest, "connector.evidence", json!({}), cancel)
    }
}
fn exchange_bounded(
    (input, output): (&mut impl Write, &mut impl Read),
    id: u64,
    method: &str,
    params: Value,
    deadline: Instant,
    cancel: &CancellationToken,
    frame_limit: usize,
) -> Result<Value, FabricError> {
    let mut request =
        serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .map_err(|_| failure("invalid_request"))?;
    request.push(b'\n');
    if request.len() > frame_limit {
        return Err(failure("request_too_large"));
    }
    let mut sent = 0;
    let mut frame = Vec::new();
    loop {
        if cancel.is_cancelled() {
            return Err(failure("cancelled"));
        }
        if Instant::now() >= deadline {
            let phase = if sent < request.len() {
                "request_write"
            } else {
                "response_read"
            };
            return Err(FabricError {
                kind: ErrorKind::Output,
                message: format!(
                    "Plugin: timeout. RPC {method} (request {id}, phase {phase}, sent {sent}/{} bytes, received {} bytes).",
                    request.len(),
                    frame.len()
                ),
            });
        }
        if sent < request.len() {
            match input.write(&request[sent..]) {
                Ok(0) => return Err(failure("closed_input")),
                Ok(n) => sent += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(failure("write_failed")),
            }
        } else {
            // Read at most one frame, never consuming bytes belonging to another response.
            let mut byte = [0];
            match output.read(&mut byte) {
                Ok(0) => return Err(failure("closed_output")),
                Ok(_) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    if frame.len() >= frame_limit - 1 {
                        return Err(failure("frame_too_large"));
                    }
                    frame.push(byte[0]);
                    continue;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(failure("read_failed")),
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let response: Response =
        serde_json::from_slice(&frame).map_err(|_| failure("malformed_response"))?;
    if response.jsonrpc != "2.0"
        || response.id != id
        || response.result.is_some() == response.error.is_some()
    {
        return Err(failure("invalid_response"));
    }
    if let Some(error) = response.error {
        let _ = (error.code, error.message);
        return Err(failure("remote_error"));
    }
    response.result.ok_or_else(|| failure("missing_result"))
}

pub struct PluginHarness {
    manifest: PluginManifest,
    normalizer: std::sync::Mutex<Option<PluginSession>>,
}
impl Drop for PluginHarness {
    fn drop(&mut self) {
        if let Ok(slot) = self.normalizer.get_mut() {
            if let Some(session) = slot.take() {
                let _ = session.finish();
            }
        }
    }
}
impl PluginHarness {
    pub fn new(manifest: PluginManifest) -> Result<Self, FabricError> {
        manifest.validate()?;
        if manifest.kind != PluginKind::Harness || !manifest.compatible() {
            return Err(failure("incompatible_harness"));
        }
        Ok(Self {
            manifest,
            normalizer: std::sync::Mutex::new(None),
        })
    }
}
impl HarnessAdapter for PluginHarness {
    fn id(&self) -> HarnessId {
        self.manifest.id.clone()
    }
    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        PluginHost::default().harness_info(&self.manifest, &CancellationToken::default())
    }
    fn native_context(&self) -> Result<crate::NativeContext, FabricError> {
        if self.manifest.native.is_some() {
            crate::native_plugin::native_context(&self.manifest)
        } else {
            Err(FabricError {
                kind: ErrorKind::UnsupportedCapability,
                message: "This plugin does not provide native-context onboarding.".into(),
            })
        }
    }
    fn plan(
        &self,
        request: &fabric_domain::HarnessRequest,
        info: &HarnessInfo,
    ) -> Result<fabric_domain::HarnessPlan, FabricError> {
        if self.manifest.native.is_some() {
            crate::native_plugin::plan(&self.manifest, request, info)
        } else {
            crate::negotiate(request, self.capabilities(info)?, false)
        }
    }
    fn try_normalize_event(
        &self,
        event: crate::NativeEvent<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Option<fabric_domain::AgentEvent>, FabricError> {
        if self.manifest.native.is_none() {
            return Ok(None);
        }
        let mut slot = self
            .normalizer
            .lock()
            .map_err(|_| failure("normalizer_poisoned"))?;
        if slot.is_none() {
            *slot = Some(PluginSession::open(
                &self.manifest,
                Duration::from_secs(5),
                cancellation,
            )?);
        }
        slot.as_mut().unwrap().set_cancellation(cancellation);
        let result = crate::native_plugin::normalize(slot.as_mut().unwrap(), event);
        if result.is_err() {
            slot.take();
        }
        result
    }
    fn capabilities(
        &self,
        info: &HarnessInfo,
    ) -> Result<fabric_domain::HarnessCapabilityManifest, FabricError> {
        if self.manifest.native.is_some() {
            return crate::native_plugin::capabilities(&self.manifest, info);
        }
        crate::planning::local_manifest(
            self.id(),
            info,
            &self.manifest.version,
            fabric_domain::CapabilitySupport::Native,
        )
    }
    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        self.manifest.verify_integrity()?;
        if self.manifest.native.is_some() {
            return crate::native_plugin::command(&self.manifest, spec);
        }
        crate::profile::revalidate(self, spec, &self.manifest.version)?;
        // No plugin-supplied argv, cwd, environment or deletion request enters Core.
        let mut args: Vec<_> = self
            .manifest
            .args
            .iter()
            .map(std::ffi::OsString::from)
            .collect();
        args.extend(["--fabric-plugin-run".into(), spec.prompt_file.into()]);
        Ok(CommandSpec {
            program: self.manifest.executable.clone(),
            args,
            environment: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn blocked_request_write_obeys_exchange_deadline() {
        let (mut input, _peer) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        loop {
            match input.write(&[0; 4096]) {
                Ok(0) => panic!("unexpected closed socket"),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        let mut output = input.try_clone().unwrap();
        let start = Instant::now();
        let error = exchange_bounded(
            (&mut input, &mut output),
            1,
            "initialize",
            json!({}),
            start + Duration::from_millis(50),
            &CancellationToken::default(),
            MAX_FRAME,
        )
        .unwrap_err();
        assert!(error.message.contains("timeout"));
        assert!(
            error
                .message
                .contains("RPC initialize (request 1, phase request_write, sent 0/")
        );
        assert!(error.message.contains("received 0 bytes"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
