//! Generic target-owned execution and bounded controller transfer staging.
use fabric_domain::{EnvironmentSelection, FabricError, HarnessId, HarnessPlan, HarnessRequest};
use fabric_runner::target::{Target, TargetPin};
use fabric_runner::{AgentSpec, CancellationToken, CommandSpec, HarnessAdapter, HarnessInfo};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub(crate) const TRANSFER_LIMIT: u64 = 134_217_728;
pub(crate) const CHUNK: usize = 8192;
pub(crate) fn error(_: impl std::fmt::Display) -> FabricError {
    fabric_runner::execution_error("Remote target operation failed; no local execution fallback")
}
pub(crate) fn protocol_error(reason: impl std::fmt::Display) -> FabricError {
    let mut failure = fabric_runner::execution_error(format!("Remote target protocol: {reason}"));
    failure.kind = fabric_domain::ErrorKind::TargetProtocol;
    failure
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Completion {
    pub resource: Value,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub stop_reason: Option<String>,
    pub harness_failed: bool,
    pub capture: fabric_domain::CaptureMetadata,
}
impl Completion {
    pub fn reason(&self) -> Option<&str> {
        self.stop_reason.as_deref().or_else(|| {
            (self.harness_failed || self.capture.native_failure.is_some())
                .then_some("native_failure")
        })
    }
}
pub(crate) fn completion(value: Value, resource: &Value) -> Result<Completion, FabricError> {
    let keys = [
        "resource",
        "exit_code",
        "exit_signal",
        "stop_reason",
        "harness_failed",
        "capture",
    ];
    if !value
        .as_object()
        .is_some_and(|o| o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)))
    {
        return Err(protocol_error("incomplete result"));
    }
    let result: Completion = serde_json::from_value(value).map_err(protocol_error)?;
    let capture = &result.capture;
    let sum = capture
        .stdout
        .size_bytes
        .checked_add(capture.stderr.size_bytes)
        .and_then(|n| n.checked_add(capture.events.size_bytes));
    if result.resource != *resource
        || result.exit_code.is_some() == result.exit_signal.is_some()
        || result.exit_code.is_some_and(|n| !(0..=255).contains(&n))
        || result.exit_signal.is_some_and(|n| !(1..=64).contains(&n))
        || result.stop_reason.as_deref().is_some_and(|r| {
            ![
                "timeout",
                "cancelled",
                "workspace_limit",
                "storage_pressure",
                "storage_meter_failed",
            ]
            .contains(&r)
        })
        || capture.limit_bytes > TRANSFER_LIMIT
        || sum != Some(capture.persisted_bytes)
        || capture.persisted_bytes > capture.limit_bytes
    {
        return Err(protocol_error("invalid remote completion"));
    }
    Ok(result)
}

/// Validate every durable completion field available, including older runs that
/// predate the complete remote-result receipt.
pub(crate) fn validate_completion(
    store: &fabric_store::Store,
    run: &fabric_store::Run,
    result: &Completion,
) -> Result<(), FabricError> {
    let value = serde_json::to_value(result).map_err(error)?;
    if store
        .remote_completion(&run.run_id)
        .map_err(error)?
        .is_some_and(|saved| saved != value)
        || ((run.exit_code.is_some() || run.exit_signal.is_some())
            && (run.exit_code != result.exit_code || run.exit_signal != result.exit_signal))
        || run
            .capture
            .as_ref()
            .is_some_and(|capture| capture != &result.capture)
    {
        return Err(protocol_error("durable remote completion changed"));
    }
    for attempt in store.worker_attempts(&run.run_id).map_err(error)? {
        if attempt.finished_at.is_some()
            && (attempt.exit_code != result.exit_code
                || attempt.exit_signal != result.exit_signal
                || attempt.termination_reason.as_deref() != result.reason()
                || attempt.event_cursor != result.capture.normalized_event_count)
        {
            return Err(protocol_error("durable remote worker outcome changed"));
        }
    }
    if run.error_kind.as_deref().is_some_and(|reason| {
        matches!(
            reason,
            "timeout"
                | "cancelled"
                | "workspace_limit"
                | "storage_pressure"
                | "storage_meter_failed"
                | "native_failure"
        ) && Some(reason) != result.reason()
    }) {
        return Err(protocol_error("durable remote stop reason changed"));
    }
    Ok(())
}

pub(crate) fn recovery_binding(
    store: &fabric_store::Store,
    run: &fabric_store::Run,
) -> Result<fabric_store::LaunchReceipt, FabricError> {
    let receipt = store
        .launch_receipt(&run.run_id)
        .map_err(error)?
        .ok_or_else(|| error("missing receipt"))?;
    let remote = receipt
        .remote
        .as_ref()
        .ok_or_else(|| error("missing remote receipt"))?;
    let workspace = store
        .workspace(&run.workspace_id)
        .map_err(error)?
        .ok_or_else(|| error("missing workspace"))?;
    let pin = store
        .run_target(&run.run_id)
        .map_err(error)?
        .ok_or_else(|| error("missing target"))?;
    let prepared = store
        .prepared_state(&run.prepared_state_id)
        .map_err(error)?
        .ok_or_else(|| error("missing source"))?;
    let source = store
        .source_evidence(&run.prepared_state_id)
        .map_err(error)?
        .ok_or_else(|| error("missing source evidence"))?;
    let environment = store
        .environment(&remote.environment_id)
        .map_err(error)?
        .ok_or_else(|| error("missing environment"))?;
    if remote.target_json != pin
        || receipt.run_id != run.run_id
        || receipt.workspace_id != workspace.workspace_id
        || receipt.prepared_state_id != run.prepared_state_id
        || receipt.source_commit_sha != run.base_commit_sha
        || receipt.source_commit_sha != prepared.source_commit_sha
        || receipt.preparation_key != prepared.preparation_key
        || receipt.recipe_digest != prepared.recipe_digest
        || receipt.source != source
        || receipt.actual_sha256 != source.prepared_sha256
        || remote.resource["run_id"] != run.run_id.as_str()
        || remote.resource["instance"] != remote.instance
        || remote.resource["resource_id"] != receipt.workspace_path
        || workspace.run_id != run.run_id
        || workspace.state_id != run.prepared_state_id
        || environment.target_json != pin
        || environment.destroy_intent
        || environment.owner_uid != unsafe { libc::geteuid() }
        || remote.resource["environment"] != crate::environment::remote_input(&environment)?
    {
        return Err(protocol_error("remote recovery binding changed"));
    }
    Ok(receipt)
}
pub(crate) fn verify_output(
    container: &Path,
    capture: &fabric_domain::CaptureMetadata,
) -> Result<(), FabricError> {
    for (name, expected) in [
        ("stdout.log", &capture.stdout),
        ("stderr.log", &capture.stderr),
        ("events.jsonl", &capture.events),
    ] {
        let path = container.join(name);
        let meta = fs::symlink_metadata(&path).map_err(error)?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.len() != expected.size_bytes
            || digest(&path)? != expected.sha256
        {
            return Err(protocol_error("capture mismatch"));
        }
    }
    Ok(())
}
pub(crate) fn private_file(path: &Path) -> Result<File, FabricError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(error)
}
pub(crate) fn digest(path: &Path) -> Result<String, FabricError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(error)?;
    let meta = file.metadata().map_err(error)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > TRANSFER_LIMIT {
        return Err(error("unsafe transfer"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(error)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Only fresh, inactive, private staging directories may be passed here. No archive
/// path, link, owner, device or permission is authority over controller storage.
pub(crate) fn unpack(file: File, destination: &Path, limit: u64) -> Result<(), FabricError> {
    // Distinguish decoder errors caused by target bytes from local file-read I/O.
    struct LocalReads<'a>(File, &'a std::cell::Cell<bool>);
    impl Read for LocalReads<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let result = self.0.read(buffer);
            if result.is_err() {
                self.1.set(true);
            }
            result
        }
    }
    let read_failed = std::cell::Cell::new(false);
    let decode = |cause: std::io::Error| {
        if read_failed.get() {
            error(cause)
        } else {
            protocol_error(cause)
        }
    };
    let mut archive = tar::Archive::new(LocalReads(file, &read_failed));
    let mut paths = HashSet::new();
    let mut links = Vec::new();
    let mut total = 0u64;
    for entry in archive.entries().map_err(&decode)? {
        let mut entry = entry.map_err(&decode)?;
        let relative = entry.path().map_err(&decode)?.into_owned();
        if relative.as_os_str().is_empty()
            || relative.components().count() > 64
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || !paths.insert(relative.clone())
            || paths.len() > 100_000
        {
            return Err(protocol_error("unsafe archive path"));
        }
        let target = destination.join(&relative);
        let parent = target.parent().ok_or_else(|| error("missing parent"))?;
        let mut current = destination.to_path_buf();
        for part in relative.parent().unwrap_or(Path::new("")).components() {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() => (),
                Ok(_) => return Err(protocol_error("link parent")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(protocol_error("parent absent"));
                }
                Err(e) => return Err(error(e)),
            }
        }
        if !parent.is_dir() {
            return Err(protocol_error("parent absent"));
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir(&target).map_err(error)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).map_err(error)?;
        } else if kind.is_file() {
            let size = entry.size();
            total = total
                .checked_add(size)
                .ok_or_else(|| protocol_error("overflow"))?;
            if total > limit.min(TRANSFER_LIMIT) {
                return Err(protocol_error("transfer budget"));
            }
            let mode = if entry.header().mode().map_err(&decode)? & 0o100 != 0 {
                0o700
            } else {
                0o600
            };
            let mut output = private_file(&target)?;
            let mut copied = 0;
            let mut buffer = [0; 65536];
            loop {
                let n = entry.read(&mut buffer).map_err(&decode)?;
                if n == 0 {
                    break;
                }
                output.write_all(&buffer[..n]).map_err(error)?;
                copied += n as u64;
            }
            if copied != size {
                return Err(protocol_error("short file"));
            }
            output
                .set_permissions(fs::Permissions::from_mode(mode))
                .map_err(error)?;
            output.sync_all().map_err(error)?;
        } else if kind.is_symlink() {
            let link = entry
                .link_name()
                .map_err(&decode)?
                .ok_or_else(|| protocol_error("missing link"))?
                .into_owned();
            if link.is_absolute() {
                return Err(protocol_error("absolute link"));
            }
            let mut depth = relative
                .parent()
                .unwrap_or(Path::new(""))
                .components()
                .count();
            for component in link.components() {
                match component {
                    Component::Normal(_) => depth += 1,
                    Component::CurDir => (),
                    Component::ParentDir if depth > 1 => depth -= 1,
                    _ => return Err(protocol_error("escaping tree link")),
                }
            }
            links.push((target, link));
        } else {
            return Err(protocol_error("special or hard link"));
        }
    }
    for (path, link) in links {
        std::os::unix::fs::symlink(link, path).map_err(error)?;
    }
    Ok(())
}

pub(crate) fn archive(
    paths: &[(&Path, &Path)],
    output: File,
    limit: u64,
) -> Result<File, FabricError> {
    fn append(
        builder: &mut tar::Builder<File>,
        path: &Path,
        name: &Path,
        bytes: &mut u64,
        entries: &mut usize,
        limit: u64,
    ) -> Result<(), FabricError> {
        *entries += 1;
        if *entries > 100_000 || name.components().count() > 64 {
            return Err(error("tree bounds"));
        }
        let meta = fs::symlink_metadata(path).map_err(error)?;
        if meta.uid() != unsafe { libc::geteuid() } || (!meta.is_dir() && meta.nlink() != 1) {
            return Err(error("foreign or linked file"));
        }
        if meta.is_dir() {
            builder.append_dir(name, path).map_err(error)?;
            let mut children = Vec::new();
            for child in fs::read_dir(path).map_err(error)? {
                if children.len() + *entries >= 100_000 {
                    return Err(error("tree bounds"));
                }
                children.push(child.map_err(error)?);
            }
            children.sort_by_key(|e| e.file_name());
            for child in children {
                append(
                    builder,
                    &child.path(),
                    &name.join(child.file_name()),
                    bytes,
                    entries,
                    limit,
                )?;
            }
        } else if meta.is_file() || meta.file_type().is_symlink() {
            *bytes = bytes
                .checked_add(meta.len())
                .ok_or_else(|| error("overflow"))?;
            if *bytes > limit.min(TRANSFER_LIMIT) {
                return Err(error("transfer budget"));
            }
            builder.append_path_with_name(path, name).map_err(error)?;
        } else {
            return Err(error("special file"));
        }
        Ok(())
    }
    let mut builder = tar::Builder::new(output);
    builder.follow_symlinks(false);
    let (mut bytes, mut entries) = (0, 0);
    for (path, name) in paths {
        append(&mut builder, path, name, &mut bytes, &mut entries, limit)?;
    }
    let output = builder.into_inner().map_err(error)?;
    output.sync_all().map_err(error)?;
    if output.metadata().map_err(error)?.len() > TRANSFER_LIMIT {
        return Err(error("archive budget"));
    }
    Ok(output)
}

pub(crate) fn binding(root: &Path, selection: &EnvironmentSelection) -> Result<Value, FabricError> {
    let (_, store) = crate::workspace::open_store(root)?;
    let record = store
        .environment(&selection.environment_id)
        .map_err(error)?
        .ok_or_else(|| error("environment absent"))?;
    if record.owner != selection.owner
        || record.owner_uid != unsafe { libc::geteuid() }
        || record.destroy_intent
    {
        return Err(error("owner mismatch"));
    }
    crate::environment::remote_input(&record)
}

struct RemoteHarness {
    id: HarnessId,
    pin: TargetPin,
    environment: Value,
}
impl RemoteHarness {
    fn call(&self, operation: &str, input: Value) -> Result<Value, FabricError> {
        let target = self.pin.open(&CancellationToken::default())?;
        let result = target.remote_call(
            operation,
            json!({"environment":self.environment,"harness":self.id,"input":input}),
        );
        let finished = target.finish();
        let value = result?;
        finished?;
        Ok(value)
    }
}
impl HarnessAdapter for RemoteHarness {
    fn id(&self) -> HarnessId {
        self.id.clone()
    }
    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let reply = self.call("harness.probe", json!({}))?;
        let version = reply["version"]
            .as_str()
            .ok_or_else(|| error("version absent"))?
            .to_owned();
        fabric_domain::HarnessVersion::validate(&version)?;
        Ok(HarnessInfo { version })
    }
    fn plan(
        &self,
        request: &HarnessRequest,
        info: &HarnessInfo,
    ) -> Result<HarnessPlan, FabricError> {
        let plan: HarnessPlan = serde_json::from_value(self.call(
            "harness.plan",
            json!({"request":request,"version":info.version}),
        )?)
        .map_err(error)?;
        if plan.manifest.harness_id != self.id
            || plan.manifest.exact_version != info.version
            || plan.request.placement != self.pin.placement
        {
            return Err(error("plan binding"));
        }
        Ok(plan)
    }
    fn command(&self, _: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        Err(error("remote harness cannot launch on controller"))
    }
}

/// Resolves an adapter in its actual execution environment, without probing local
/// native config/auth or applying controller onboarding defaults to a remote HOME.
pub fn resolve_harness(
    root: &Path,
    id: &HarnessId,
    options: &crate::run::RunOptions,
    registry: &fabric_runner::HarnessRegistry,
    registry_options: &fabric_runner::RegistryOptions,
) -> Result<Box<dyn HarnessAdapter>, FabricError> {
    let pin = match &options.target_pin {
        Some(pin) => pin.clone(),
        None => TargetPin::select(&options.harness_request.placement)?,
    };
    let target = pin.open(&options.cancellation)?;
    let remote = target.is_remote();
    target.finish()?;
    if remote {
        if registry_options.fake_binary.is_some() {
            return Err(error("controller binary cannot be imported"));
        }
        let selection = options
            .environment
            .as_ref()
            .ok_or_else(|| error("remote execution requires explicit environment"))?;
        Ok(Box::new(RemoteHarness {
            id: id.clone(),
            pin,
            environment: binding(root, selection)?,
        }))
    } else {
        registry.resolve(id, registry_options)
    }
}

pub(crate) fn is_remote(options: &crate::run::RunOptions) -> Result<bool, FabricError> {
    let pin = match &options.target_pin {
        Some(p) => p.clone(),
        None => TargetPin::select(&options.harness_request.placement)?,
    };
    let target = pin.open(&options.cancellation)?;
    let remote = target.is_remote();
    target.finish()?;
    Ok(remote)
}

pub(crate) fn upload(
    target: &Target,
    environment: &Value,
    run: &str,
    tree: &Path,
    limit: u64,
) -> Result<Value, FabricError> {
    let parent = tree.parent().ok_or_else(|| error("container absent"))?;
    let path = parent.join("source.transfer.tar");
    drop(archive(
        &[
            (tree, Path::new("tree")),
            (&parent.join("prompt.input"), Path::new("prompt.input")),
        ],
        private_file(&path)?,
        limit,
    )?);
    let sha256 = digest(&path)?;
    let size = fs::metadata(&path).map_err(error)?.len();
    let resource = target.remote_call("workspace.create", json!({"environment":environment,"run_id":run,"size":size,"sha256":sha256,"limit":limit.min(TRANSFER_LIMIT)}))?;
    let mut input = File::open(&path).map_err(error)?;
    let mut buffer = [0; CHUNK];
    let mut offset = 0u64;
    loop {
        let n = input.read(&mut buffer).map_err(error)?;
        if n == 0 {
            break;
        }
        let reply = target.remote_call(
            "source.write",
            json!({"resource":resource,"offset":offset,"bytes":&buffer[..n]}),
        )?;
        offset += n as u64;
        if reply["offset"].as_u64() != Some(offset) {
            return Err(protocol_error("source offset"));
        }
    }
    let reply = target.remote_call("source.seal", json!({"resource":resource}))?;
    fs::remove_file(path).map_err(error)?;
    Ok(json!({"resource":resource,"witness":reply}))
}

pub(crate) struct Staging {
    pub path: PathBuf,
    pub witness: Value,
    retained: bool,
}
impl Staging {
    pub fn new(parent: &Path) -> Result<Self, FabricError> {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(error)?
            .as_nanos();
        let path = parent.join(format!(
            ".fabric-transfer-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(error)?;
        Ok(Self {
            path,
            witness: Value::Null,
            retained: false,
        })
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.retained {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(crate) fn download_staged(
    target: &Target,
    resource: &Value,
    parent: &Path,
    limit: u64,
) -> Result<Staging, FabricError> {
    let mut staging = Staging::new(parent)?;
    let export = target.remote_call(
        "workspace.export",
        json!({"resource":resource,"limit":limit.min(TRANSFER_LIMIT)}),
    )?;
    let size = export["size"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= limit.min(TRANSFER_LIMIT))
        .ok_or_else(|| protocol_error("export bounds"))?;
    let path = staging.path.join("result.transfer.tar");
    let mut output = private_file(&path)?;
    let mut offset = 0;
    while offset < size {
        let reply = target.remote_call(
            "workspace.read",
            json!({"resource":resource,"offset":offset}),
        )?;
        let bytes: Vec<u8> =
            serde_json::from_value(reply["bytes"].clone()).map_err(protocol_error)?;
        if bytes.is_empty() || bytes.len() > CHUNK || offset + bytes.len() as u64 > size {
            return Err(protocol_error("export chunk"));
        }
        output.write_all(&bytes).map_err(error)?;
        offset += bytes.len() as u64;
    }
    output.sync_all().map_err(error)?;
    drop(output);
    if digest(&path)?
        != export["sha256"]
            .as_str()
            .ok_or_else(|| protocol_error("export digest"))?
    {
        return Err(protocol_error("export hash mismatch"));
    }
    unpack(File::open(&path).map_err(error)?, &staging.path, limit)?;
    fs::remove_file(path).map_err(error)?;
    for (name, directory) in [
        ("tree", true),
        ("stdout.log", false),
        ("stderr.log", false),
        ("events.jsonl", false),
    ] {
        match fs::symlink_metadata(staging.path.join(name)) {
            Ok(meta)
                if if directory {
                    meta.is_dir()
                } else {
                    meta.is_file()
                } => {}
            Ok(_) => return Err(protocol_error(format!("returned {name} type"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(protocol_error(format!("missing {name}")));
            }
            Err(e) => return Err(error(e)),
        }
    }
    if fs::read_dir(&staging.path).map_err(error)?.count() != 4 {
        return Err(protocol_error("unexpected export entry"));
    }
    staging.witness = export["witness"].clone();
    Ok(staging)
}
const RETURNED: [&str; 4] = ["tree", "stdout.log", "stderr.log", "events.jsonl"];

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
    created: Option<std::time::SystemTime>,
}
fn identity(path: &Path, directory: bool) -> Result<ObjectIdentity, FabricError> {
    let meta = fs::symlink_metadata(path).map_err(error)?;
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file() || meta.nlink() != 1
        }
    {
        return Err(error("unsafe local installation object"));
    }
    Ok(ObjectIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        created: meta.created().ok(),
    })
}
fn exists(path: &Path) -> Result<bool, FabricError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(error(e)),
    }
}
fn tree_digest(path: &Path) -> Result<String, FabricError> {
    fabric_project::prepare::content_digest(
        path,
        std::time::Instant::now() + std::time::Duration::from_secs(1200),
    )
}

/// Core-created, immutable database receipt. No directory name or matching bytes
/// alone authorize resuming an installation.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Installation {
    stage: String,
    stage_identity: ObjectIdentity,
    original: ObjectIdentity,
    source_sha256: String,
    returned: [ObjectIdentity; 4],
    tree_sha256: String,
}
impl Installation {
    pub fn load(
        store: &fabric_store::Store,
        run: &fabric_store::Run,
    ) -> Result<Option<Self>, FabricError> {
        store
            .remote_installation(&run.run_id)
            .map_err(error)?
            .map(|value| serde_json::from_value(value).map_err(error))
            .transpose()
    }

    fn prepare(
        staging: &Staging,
        container: &Path,
        source_sha256: &str,
    ) -> Result<Self, FabricError> {
        for name in [
            "stdout.log",
            "stderr.log",
            "events.jsonl",
            "transfer-previous",
        ] {
            if exists(&container.join(name))? {
                return Err(error("existing partial download; retain evidence"));
            }
        }
        if staging.path.parent() != Some(container)
            || tree_digest(&container.join("tree"))? != source_sha256
        {
            return Err(error("local source staging changed"));
        }
        File::open(&staging.path)
            .and_then(|f| f.sync_all())
            .map_err(error)?;
        File::open(container)
            .and_then(|f| f.sync_all())
            .map_err(error)?;
        Ok(Self {
            stage: staging
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| error("stage name"))?
                .to_owned(),
            stage_identity: identity(&staging.path, true)?,
            original: identity(&container.join("tree"), true)?,
            source_sha256: source_sha256.to_owned(),
            returned: [
                identity(&staging.path.join("tree"), true)?,
                identity(&staging.path.join("stdout.log"), false)?,
                identity(&staging.path.join("stderr.log"), false)?,
                identity(&staging.path.join("events.jsonl"), false)?,
            ],
            tree_sha256: tree_digest(&staging.path.join("tree"))?,
        })
    }

    /// Read-only validation also accepts the gap between the two tree renames.
    pub fn validate(
        &self,
        container: &Path,
        source_sha256: &str,
        capture: &fabric_domain::CaptureMetadata,
    ) -> Result<[bool; 4], FabricError> {
        if self.source_sha256 != source_sha256
            || !self.stage.starts_with(".fabric-transfer-")
            || Path::new(&self.stage).components().count() != 1
            || !matches!(
                Path::new(&self.stage).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(error("invalid installation receipt"));
        }
        let stage = container.join(&self.stage);
        if identity(&stage, true)? != self.stage_identity {
            return Err(error("installation staging replaced"));
        }
        let tree = container.join("tree");
        let previous = container.join("transfer-previous");
        let original_at_tree = exists(&tree)? && identity(&tree, true)? == self.original;
        let original = if original_at_tree {
            if exists(&previous)? {
                return Err(error("foreign previous tree"));
            }
            tree.clone()
        } else {
            previous
        };
        if identity(&original, true)? != self.original
            || tree_digest(&original)? != self.source_sha256
        {
            return Err(error("original transfer tree changed"));
        }
        let mut installed = [false; 4];
        for (i, name) in RETURNED.iter().enumerate() {
            let source = stage.join(name);
            let destination = container.join(name);
            let at_source = exists(&source)?;
            let at_destination = exists(&destination)? && !(i == 0 && original_at_tree);
            if at_source == at_destination {
                return Err(error("missing or duplicate installation object"));
            }
            let path = if at_source { source } else { destination };
            if identity(&path, i == 0)? != self.returned[i] {
                return Err(error("installation object replaced"));
            }
            if i == 0 {
                if tree_digest(&path)? != self.tree_sha256 {
                    return Err(error("installed tree changed"));
                }
            } else {
                let expected = [&capture.stdout, &capture.stderr, &capture.events][i - 1];
                if fs::symlink_metadata(&path).map_err(error)?.len() != expected.size_bytes
                    || digest(&path)? != expected.sha256
                {
                    return Err(error("installed log changed"));
                }
            }
            installed[i] = at_destination;
        }
        if installed.windows(2).any(|pair| !pair[0] && pair[1])
            || original_at_tree && installed.iter().any(|v| *v)
        {
            return Err(error("invalid installation rename order"));
        }
        Ok(installed)
    }

    fn finish(
        &self,
        container: &Path,
        capture: &fabric_domain::CaptureMetadata,
        mut after_rename: impl FnMut() -> Result<(), FabricError>,
    ) -> Result<(), FabricError> {
        let installed = self.validate(container, &self.source_sha256, capture)?;
        if !exists(&container.join("transfer-previous"))? {
            fs::rename(container.join("tree"), container.join("transfer-previous"))
                .map_err(error)?;
            File::open(container)
                .and_then(|f| f.sync_all())
                .map_err(error)?;
            after_rename()?;
        }
        for (i, name) in RETURNED.iter().enumerate() {
            if !installed[i] {
                fs::rename(container.join(&self.stage).join(name), container.join(name))
                    .map_err(error)?;
                File::open(container.join(&self.stage))
                    .and_then(|f| f.sync_all())
                    .map_err(error)?;
                File::open(container)
                    .and_then(|f| f.sync_all())
                    .map_err(error)?;
                after_rename()?;
            }
        }
        Ok(())
    }
}

pub(crate) fn install_download(
    staging: &mut Staging,
    container: &Path,
    store: &fabric_store::Store,
    run: &fabric_store::Run,
    source_sha256: &str,
    capture: &fabric_domain::CaptureMetadata,
) -> Result<(), FabricError> {
    let installation = if let Some(saved) = Installation::load(store, run)? {
        saved
    } else {
        let installation = Installation::prepare(staging, container, source_sha256)?;
        store
            .record_remote_installation(
                &run.run_id,
                &serde_json::to_value(&installation).map_err(error)?,
            )
            .map_err(error)?;
        // From the commit onward, even a failed rename must retain this exact staging.
        staging.retained = true;
        installation
    };
    if installation.source_sha256 != source_sha256 {
        return Err(error("installation source changed"));
    }
    installation.finish(container, capture, || Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn entry(
        builder: &mut tar::Builder<File>,
        path: &str,
        kind: tar::EntryType,
        link: Option<&str>,
        bytes: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o600);
        header.set_size(bytes.len() as u64);
        if let Some(link) = link {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(bytes))
            .unwrap();
    }

    #[test]
    fn returned_bytes_are_exact_and_links_cannot_escape_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("input.tar");
        let mut builder = tar::Builder::new(private_file(&archive).unwrap());
        entry(&mut builder, "tree", tar::EntryType::Directory, None, b"");
        entry(
            &mut builder,
            "tree/proof",
            tar::EntryType::Regular,
            None,
            b"\0\xffremote\n",
        );
        entry(
            &mut builder,
            "tree/link",
            tar::EntryType::Symlink,
            Some("proof"),
            b"",
        );
        drop(builder.into_inner().unwrap());
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        unpack(File::open(&archive).unwrap(), &output, 9).unwrap();
        assert_eq!(
            fs::read(output.join("tree/proof")).unwrap(),
            b"\0\xffremote\n"
        );
        assert_eq!(
            fs::read_link(output.join("tree/link")).unwrap(),
            Path::new("proof")
        );
        for (name, kind, link) in [
            ("escape", tar::EntryType::Symlink, Some("../outside")),
            ("absolute", tar::EntryType::Symlink, Some("/outside")),
            ("hard", tar::EntryType::Link, Some("tree/proof")),
            ("fifo", tar::EntryType::Fifo, None),
        ] {
            let path = temp.path().join(format!("{name}.tar"));
            let mut builder = tar::Builder::new(private_file(&path).unwrap());
            entry(&mut builder, "tree", tar::EntryType::Directory, None, b"");
            entry(&mut builder, &format!("tree/{name}"), kind, link, b"");
            drop(builder.into_inner().unwrap());
            let destination = temp.path().join(name);
            fs::create_dir(&destination).unwrap();
            assert_eq!(
                unpack(File::open(path).unwrap(), &destination, 100)
                    .unwrap_err()
                    .kind,
                fabric_domain::ErrorKind::TargetProtocol
            );
        }
        assert!(!temp.path().join("outside").exists());
    }

    #[test]
    fn duplicate_archive_names_and_byte_budget_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        for duplicate in [false, true] {
            let path = temp.path().join(format!("{duplicate}.tar"));
            let mut builder = tar::Builder::new(private_file(&path).unwrap());
            entry(&mut builder, "tree", tar::EntryType::Directory, None, b"");
            entry(
                &mut builder,
                "tree/file",
                tar::EntryType::Regular,
                None,
                b"1234",
            );
            if duplicate {
                entry(
                    &mut builder,
                    "tree/file",
                    tar::EntryType::Regular,
                    None,
                    b"5678",
                );
            }
            drop(builder.into_inner().unwrap());
            let destination = temp.path().join(format!("{duplicate}"));
            fs::create_dir(&destination).unwrap();
            assert_eq!(
                unpack(
                    File::open(path).unwrap(),
                    &destination,
                    if duplicate { 100 } else { 3 }
                )
                .unwrap_err()
                .kind,
                fabric_domain::ErrorKind::TargetProtocol
            );
        }
    }

    fn installation_fixture(
        container: &Path,
    ) -> (Staging, Installation, fabric_domain::CaptureMetadata) {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(container.join("tree"))
            .unwrap();
        private_file(&container.join("tree/original"))
            .unwrap()
            .write_all(b"source")
            .unwrap();
        let staging = Staging::new(container).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(staging.path.join("tree"))
            .unwrap();
        private_file(&staging.path.join("tree/result"))
            .unwrap()
            .write_all(b"retained result")
            .unwrap();
        for name in &RETURNED[1..] {
            private_file(&staging.path.join(name))
                .unwrap()
                .write_all(name.as_bytes())
                .unwrap();
        }
        let capture = serde_json::from_value(json!({
            "limit_bytes":1024,"persisted_bytes":32,"truncated":false,
            "stdout_bytes_seen":10,"stderr_bytes_seen":10,"event_counts":{"parsed":0,"invalid":0,"oversized":0},
            "stdout":{"size_bytes":10,"sha256":digest(&staging.path.join("stdout.log")).unwrap()},
            "stderr":{"size_bytes":10,"sha256":digest(&staging.path.join("stderr.log")).unwrap()},
            "events":{"size_bytes":12,"sha256":digest(&staging.path.join("events.jsonl")).unwrap()}
        })).unwrap();
        let installation = Installation::prepare(
            &staging,
            container,
            &tree_digest(&container.join("tree")).unwrap(),
        )
        .unwrap();
        (staging, installation, capture)
    }

    #[test]
    fn verified_installation_resumes_at_every_tree_log_and_metadata_boundary() {
        // Intent-only; original-tree rename; returned-tree rename; each log;
        // all renames complete but capture metadata not yet committed.
        for boundary in 0..=5 {
            let container = tempfile::tempdir().unwrap();
            let (_staging, installation, capture) = installation_fixture(container.path());
            let receipt = serde_json::to_vec(&installation).unwrap();
            if boundary != 0 {
                let mut renames = 0;
                assert!(
                    installation
                        .finish(container.path(), &capture, || {
                            renames += 1;
                            if renames == boundary {
                                Err(error("controller stopped"))
                            } else {
                                Ok(())
                            }
                        })
                        .is_err()
                );
            }
            let restarted: Installation = serde_json::from_slice(&receipt).unwrap();
            let before = restarted
                .validate(container.path(), &restarted.source_sha256, &capture)
                .unwrap();
            let expected: [bool; 4] = std::array::from_fn(|i| boundary >= i + 2);
            assert_eq!(before, expected);
            restarted
                .finish(container.path(), &capture, || Ok(()))
                .unwrap();
            restarted
                .finish(container.path(), &capture, || {
                    panic!("completed installation renamed again")
                })
                .unwrap();
            assert_eq!(
                fs::read(container.path().join("tree/result")).unwrap(),
                b"retained result"
            );
            assert_eq!(
                fs::read(container.path().join("transfer-previous/original")).unwrap(),
                b"source"
            );
            for name in &RETURNED[1..] {
                assert_eq!(
                    fs::read(container.path().join(name)).unwrap(),
                    name.as_bytes()
                );
            }
        }
    }

    #[test]
    fn installation_never_adopts_identical_foreign_files_or_modified_verified_bytes() {
        for installed in [false, true] {
            for replacement in [false, true] {
                let container = tempfile::tempdir().unwrap();
                let (staging, installation, capture) = installation_fixture(container.path());
                if installed {
                    installation
                        .finish(container.path(), &capture, || Ok(()))
                        .unwrap();
                }
                let path = if installed {
                    container.path().join("stdout.log")
                } else {
                    staging.path.join("stdout.log")
                };
                if replacement {
                    fs::rename(&path, path.with_extension("foreign-original")).unwrap();
                    private_file(&path)
                        .unwrap()
                        .write_all(b"stdout.log")
                        .unwrap();
                } else {
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .write_all(b"changed!!!")
                        .unwrap();
                }
                assert!(
                    installation
                        .validate(container.path(), &installation.source_sha256, &capture)
                        .is_err()
                );
                assert!(
                    container
                        .path()
                        .join(if installed {
                            "tree/result"
                        } else {
                            "tree/original"
                        })
                        .is_file()
                );
            }
        }
    }
}
