//! Generic target-owned execution and bounded controller transfer staging.
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use fabric_domain::{FabricError, HarnessId, HarnessPlan, HarnessRequest, EnvironmentSelection};
use fabric_runner::{HarnessAdapter, HarnessInfo, AgentSpec, CommandSpec, CancellationToken};
use fabric_runner::target::{Target, TargetPin};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const TRANSFER_LIMIT: u64 = 134_217_728;
pub(crate) const CHUNK: usize = 8192;
pub(crate) fn error(_: impl std::fmt::Display) -> FabricError {
    fabric_runner::execution_error("Remote target operation failed; no local execution fallback")
}
pub(crate) fn private_file(path: &Path) -> Result<File, FabricError> {
    OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(path).map_err(error)
}
pub(crate) fn digest(path: &Path) -> Result<String, FabricError> {
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(error)?;
    let meta = file.metadata().map_err(error)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > TRANSFER_LIMIT { return Err(error("unsafe transfer")); }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop { let n = file.read(&mut buffer).map_err(error)?; if n == 0 { break; } hash.update(&buffer[..n]); }
    Ok(format!("{:x}", hash.finalize()))
}

/// Only fresh, inactive, private staging directories may be passed here. No archive
/// path, link, owner, device or permission is authority over controller storage.
pub(crate) fn unpack(file: File, destination: &Path, limit: u64) -> Result<(), FabricError> {
    let mut archive = tar::Archive::new(file);
    let mut paths = HashSet::new();
    let mut links = Vec::new();
    let mut total = 0u64;
    for entry in archive.entries().map_err(error)? {
        let mut entry = entry.map_err(error)?;
        let relative = entry.path().map_err(error)?.into_owned();
        if relative.as_os_str().is_empty() || relative.components().count() > 64
            || relative.components().any(|c| !matches!(c, Component::Normal(_)))
            || !paths.insert(relative.clone()) || paths.len() > 100_000 {
            return Err(error("unsafe archive path"));
        }
        let target = destination.join(&relative);
        let parent = target.parent().ok_or_else(|| error("missing parent"))?;
        let mut current = destination.to_path_buf();
        for part in relative.parent().unwrap_or(Path::new("")).components() {
            current.push(part);
            if !fs::symlink_metadata(&current).map_err(error)?.is_dir() { return Err(error("link parent")); }
        }
        if !parent.is_dir() { return Err(error("parent absent")); }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir(&target).map_err(error)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).map_err(error)?;
        } else if kind.is_file() {
            let size = entry.size();
            total = total.checked_add(size).ok_or_else(|| error("overflow"))?;
            if total > limit.min(TRANSFER_LIMIT) { return Err(error("transfer budget")); }
            let mode = if entry.header().mode().map_err(error)? & 0o100 != 0 { 0o700 } else { 0o600 };
            let mut output = private_file(&target)?;
            if std::io::copy(&mut entry, &mut output).map_err(error)? != size { return Err(error("short file")); }
            output.set_permissions(fs::Permissions::from_mode(mode)).map_err(error)?;
            output.sync_all().map_err(error)?;
        } else if kind.is_symlink() {
            let link = entry.link_name().map_err(error)?.ok_or_else(|| error("missing link"))?.into_owned();
            if link.is_absolute() { return Err(error("absolute link")); }
            let mut depth = relative.parent().unwrap_or(Path::new("")).components().count();
            for component in link.components() {
                match component { Component::Normal(_) => depth += 1, Component::CurDir => (), Component::ParentDir if depth > 1 => depth -= 1, _ => return Err(error("escaping tree link")) }
            }
            links.push((target, link));
        } else { return Err(error("special or hard link")); }
    }
    for (path, link) in links { std::os::unix::fs::symlink(link, path).map_err(error)?; }
    Ok(())
}

pub(crate) fn archive(paths: &[(&Path, &Path)], output: File, limit: u64) -> Result<File, FabricError> {
    fn append(builder: &mut tar::Builder<File>, path: &Path, name: &Path, bytes: &mut u64, entries: &mut usize, limit: u64) -> Result<(), FabricError> {
        *entries += 1;
        if *entries > 100_000 || name.components().count() > 64 { return Err(error("tree bounds")); }
        let meta = fs::symlink_metadata(path).map_err(error)?;
        if meta.uid() != unsafe { libc::geteuid() } || (!meta.is_dir() && meta.nlink() != 1) { return Err(error("foreign or linked file")); }
        if meta.is_dir() {
            builder.append_dir(name, path).map_err(error)?;
            let mut children = Vec::new();
            for child in fs::read_dir(path).map_err(error)? {
                if children.len() + *entries >= 100_000 { return Err(error("tree bounds")); }
                children.push(child.map_err(error)?);
            }
            children.sort_by_key(|e| e.file_name());
            for child in children { append(builder, &child.path(), &name.join(child.file_name()), bytes, entries, limit)?; }
        } else if meta.is_file() || meta.file_type().is_symlink() {
            *bytes = bytes.checked_add(meta.len()).ok_or_else(|| error("overflow"))?;
            if *bytes > limit.min(TRANSFER_LIMIT) { return Err(error("transfer budget")); }
            builder.append_path_with_name(path, name).map_err(error)?;
        } else { return Err(error("special file")); }
        Ok(())
    }
    let mut builder = tar::Builder::new(output);
    builder.follow_symlinks(false);
    let (mut bytes, mut entries) = (0, 0);
    for (path, name) in paths { append(&mut builder, path, name, &mut bytes, &mut entries, limit)?; }
    let output = builder.into_inner().map_err(error)?;
    output.sync_all().map_err(error)?;
    if output.metadata().map_err(error)?.len() > TRANSFER_LIMIT { return Err(error("archive budget")); }
    Ok(output)
}

pub(crate) fn binding(root: &Path, selection: &EnvironmentSelection) -> Result<Value, FabricError> {
    let (_, store) = crate::workspace::open_store(root)?;
    let record = store.environment(&selection.environment_id).map_err(error)?.ok_or_else(|| error("environment absent"))?;
    if record.owner != selection.owner || record.owner_uid != unsafe { libc::geteuid() } || record.destroy_intent { return Err(error("owner mismatch")); }
    crate::environment::remote_input(&record)
}

struct RemoteHarness { id: HarnessId, pin: TargetPin, environment: Value }
impl RemoteHarness {
    fn call(&self, operation: &str, input: Value) -> Result<Value, FabricError> {
        let target = self.pin.open(&CancellationToken::default())?;
        let result = target.remote_call(operation, json!({"environment":self.environment,"harness":self.id,"input":input}));
        let finished = target.finish();
        let value = result?; finished?; Ok(value)
    }
}
impl HarnessAdapter for RemoteHarness {
    fn id(&self) -> HarnessId { self.id.clone() }
    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let reply = self.call("harness.probe", json!({}))?;
        let version = reply["version"].as_str().ok_or_else(|| error("version absent"))?.to_owned();
        fabric_domain::HarnessVersion::validate(&version)?;
        Ok(HarnessInfo { version })
    }
    fn plan(&self, request: &HarnessRequest, info: &HarnessInfo) -> Result<HarnessPlan, FabricError> {
        let plan: HarnessPlan = serde_json::from_value(self.call("harness.plan", json!({"request":request,"version":info.version}))?).map_err(error)?;
        if plan.manifest.harness_id != self.id || plan.manifest.exact_version != info.version || plan.request.placement != self.pin.placement { return Err(error("plan binding")); }
        Ok(plan)
    }
    fn command(&self, _: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> { Err(error("remote harness cannot launch on controller")) }
}

/// Resolves an adapter in its actual execution environment, without probing local
/// native config/auth or applying controller onboarding defaults to a remote HOME.
pub fn resolve_harness(root: &Path, id: &HarnessId, options: &crate::run::RunOptions, registry: &fabric_runner::HarnessRegistry, registry_options: &fabric_runner::RegistryOptions) -> Result<Box<dyn HarnessAdapter>, FabricError> {
    let pin = match &options.target_pin { Some(pin) => pin.clone(), None => TargetPin::select(&options.harness_request.placement)? };
    let target = pin.open(&options.cancellation)?;
    let remote = target.is_remote(); target.finish()?;
    if remote {
        if registry_options.fake_binary.is_some() { return Err(error("controller binary cannot be imported")); }
        let selection = options.environment.as_ref().ok_or_else(|| error("remote execution requires explicit environment"))?;
        Ok(Box::new(RemoteHarness { id:id.clone(), pin, environment:binding(root, selection)? }))
    } else { registry.resolve(id, registry_options) }
}

pub(crate) fn is_remote(options: &crate::run::RunOptions) -> Result<bool, FabricError> {
    let pin = match &options.target_pin { Some(p) => p.clone(), None => TargetPin::select(&options.harness_request.placement)? };
    let target = pin.open(&options.cancellation)?;
    let remote = target.is_remote(); target.finish()?; Ok(remote)
}

pub(crate) fn upload(target: &Target, environment: &Value, run: &str, tree: &Path, limit: u64) -> Result<Value, FabricError> {
    let parent = tree.parent().ok_or_else(|| error("container absent"))?;
    let path = parent.join("source.transfer.tar");
    drop(archive(&[(tree, Path::new("tree")), (&parent.join("prompt.input"), Path::new("prompt.input"))], private_file(&path)?, limit)?);
    let sha256 = digest(&path)?;
    let size = fs::metadata(&path).map_err(error)?.len();
    let resource = target.remote_call("workspace.create", json!({"environment":environment,"run_id":run,"size":size,"sha256":sha256,"limit":limit.min(TRANSFER_LIMIT)}))?;
    let mut input = File::open(&path).map_err(error)?;
    let mut buffer = [0; CHUNK];
    let mut offset = 0u64;
    loop { let n = input.read(&mut buffer).map_err(error)?; if n == 0 { break; }
        let reply = target.remote_call("source.write", json!({"resource":resource,"offset":offset,"bytes":&buffer[..n]}))?;
        offset += n as u64;
        if reply["offset"].as_u64() != Some(offset) { return Err(error("source offset")); }
    }
    let reply = target.remote_call("source.seal", json!({"resource":resource}))?;
    fs::remove_file(path).map_err(error)?;
    Ok(json!({"resource":resource,"witness":reply}))
}

pub(crate) fn download(target: &Target, resource: &Value, container: &Path, limit: u64) -> Result<(), FabricError> {
    let export = target.remote_call("workspace.export", json!({"resource":resource,"limit":limit.min(TRANSFER_LIMIT)}))?;
    let size = export["size"].as_u64().filter(|n| *n <= TRANSFER_LIMIT).ok_or_else(|| error("export bounds"))?;
    let path = container.join("result.transfer.tar");
    let mut output = private_file(&path)?;
    let mut offset = 0;
    while offset < size {
        let reply = target.remote_call("workspace.read", json!({"resource":resource,"offset":offset}))?;
        let bytes: Vec<u8> = serde_json::from_value(reply["bytes"].clone()).map_err(error)?;
        if bytes.is_empty() || bytes.len() > CHUNK || offset + bytes.len() as u64 > size { return Err(error("export chunk")); }
        output.write_all(&bytes).map_err(error)?; offset += bytes.len() as u64;
    }
    output.sync_all().map_err(error)?; drop(output);
    if digest(&path)? != export["sha256"].as_str().ok_or_else(|| error("export digest"))? { return Err(error("export hash mismatch")); }
    let staging = container.join("remote-return");
    fs::create_dir(&staging).map_err(error)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).map_err(error)?;
    unpack(File::open(&path).map_err(error)?, &staging, limit)?;
    if !fs::symlink_metadata(staging.join("tree")).map_err(error)?.is_dir() {
        return Err(error("returned workspace is not a directory"));
    }
    // Both names are Core-derived inside the already validated private container;
    // remote names never select a controller path. No host process runs in either tree.
    let previous = container.join("transfer-previous");
    fs::rename(container.join("tree"), &previous).map_err(error)?;
    fs::rename(staging.join("tree"), container.join("tree")).map_err(error)?;
    for name in ["stdout.log", "stderr.log", "events.jsonl"] {
        let source = staging.join(name);
        if !fs::symlink_metadata(&source).map_err(error)?.is_file() { return Err(error("capture file type")); }
        fs::rename(source, container.join(name)).map_err(error)?;
    }
    fs::remove_dir_all(previous).map_err(error)?;
    fs::remove_dir(staging).map_err(error)?;
    fs::remove_file(path).map_err(error)?;
    File::open(container).and_then(|f| f.sync_all()).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn entry(builder: &mut tar::Builder<File>, path: &str, kind: tar::EntryType, link: Option<&str>, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o600);
        header.set_size(bytes.len() as u64);
        if let Some(link) = link { header.set_link_name(link).unwrap(); }
        header.set_cksum();
        builder.append_data(&mut header, path, Cursor::new(bytes)).unwrap();
    }

    #[test]
    fn returned_bytes_are_exact_and_links_cannot_escape_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("input.tar");
        let mut builder = tar::Builder::new(private_file(&archive).unwrap());
        entry(&mut builder, "tree", tar::EntryType::Directory, None, b"");
        entry(&mut builder, "tree/proof", tar::EntryType::Regular, None, b"\0\xffremote\n");
        entry(&mut builder, "tree/link", tar::EntryType::Symlink, Some("proof"), b"");
        drop(builder.into_inner().unwrap());
        let output = temp.path().join("output"); fs::create_dir(&output).unwrap();
        unpack(File::open(&archive).unwrap(), &output, 9).unwrap();
        assert_eq!(fs::read(output.join("tree/proof")).unwrap(), b"\0\xffremote\n");
        assert_eq!(fs::read_link(output.join("tree/link")).unwrap(), Path::new("proof"));
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
            let destination = temp.path().join(name); fs::create_dir(&destination).unwrap();
            assert!(unpack(File::open(path).unwrap(), &destination, 100).is_err());
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
            entry(&mut builder, "tree/file", tar::EntryType::Regular, None, b"1234");
            if duplicate { entry(&mut builder, "tree/file", tar::EntryType::Regular, None, b"5678"); }
            drop(builder.into_inner().unwrap());
            let destination = temp.path().join(format!("{duplicate}")); fs::create_dir(&destination).unwrap();
            assert!(unpack(File::open(path).unwrap(), &destination, if duplicate {100} else {3}).is_err());
        }
    }
}
