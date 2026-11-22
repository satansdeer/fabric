//! Complete non-ignored source recovery evidence; deletion requires separate GC checks.
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use fabric_domain::{
    ArtifactManifestId, CaptureMetadata, CapturedFile, ErrorKind, FabricError, RunId,
};
use fabric_runner::{CancellationToken, CommandSpec, ExecutionLimits};
use fabric_store::{Artifact, PreparedState, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workspace::{capture_workspace, private_directory, private_file};

#[derive(Serialize)]
struct Manifest<'a> {
    format_version: u32,
    manifest_id: &'a str,
    run_id: &'a str,
    workspace_id: &'a str,
    prepared_state_id: &'a str,
    base_commit_sha: &'a str,
    recipe_digest: &'a str,
    prompt_digest: &'a str,
    harness: &'a str,
    harness_version: Option<&'a str>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    stop_reason: Option<&'a str>,
    capture: &'a CaptureMetadata,
    artifact_limit_bytes: u64,
    source_scope: &'static str,
    files: &'a [Artifact],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredManifest {
    format_version: u32,
    manifest_id: String,
    run_id: String,
    workspace_id: String,
    prepared_state_id: String,
    base_commit_sha: String,
    recipe_digest: String,
    prompt_digest: String,
    harness: String,
    harness_version: Option<String>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    stop_reason: Option<String>,
    capture: RegisteredCapture,
    artifact_limit_bytes: u64,
    source_scope: String,
    files: Vec<RegisteredFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredCapture {
    limit_bytes: u64,
    persisted_bytes: u64,
    truncated: bool,
    stdout_bytes_seen: u64,
    stderr_bytes_seen: u64,
    stdout: RegisteredCapturedFile,
    stderr: RegisteredCapturedFile,
    events: RegisteredCapturedFile,
    event_counts: RegisteredEventCounts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredCapturedFile {
    size_bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredEventCounts {
    parsed: u64,
    invalid: u64,
    oversized: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredFile {
    kind: String,
    relative_path: String,
    sha256: String,
    size_bytes: u64,
}

/// Validate all durable recovery evidence immediately before reclaiming a workspace.
pub(super) fn verify_registered(
    root: &Path,
    store: &Store,
    run: &fabric_store::Run,
) -> Result<(), FabricError> {
    let expected_id = format!("a-{}", run.run_id);
    if run.artifact_manifest_id.as_ref().map(|id| id.as_str()) != Some(expected_id.as_str()) {
        return Err(error("Missing or unexpected registered artifact manifest"));
    }
    private_directory(root)?;
    if fs::canonicalize(root).map_err(error)? != root {
        return Err(error("Artifact root is not canonical"));
    }
    let artifacts = root.join("artifacts");
    let directory = artifacts.join(&expected_id);
    for path in [&artifacts, &directory] {
        private_directory(path)?;
        if fs::canonicalize(path).map_err(error)? != *path {
            return Err(error("Artifact directory has a symlink ancestor"));
        }
    }
    if fs::symlink_metadata(&directory).map_err(error)?.mode() & 0o222 != 0 {
        return Err(error("Artifact directory is not sealed"));
    }
    let files = store.artifacts(&run.run_id).map_err(error)?;
    let expected = [
        ("stdout", "stdout.log"),
        ("stderr", "stderr.log"),
        ("events", "events.jsonl"),
        ("prompt", "prompt.input"),
        ("tracked_patch", "tracked.patch"),
        ("capture_diagnostic", "git.stderr"),
        ("ignored_paths", "ignored.paths"),
        ("ignored_stderr", "ignored.stderr"),
        ("untracked_archive", "untracked.tar"),
        ("untracked_paths", "untracked.paths"),
        ("untracked_stderr", "untracked.stderr"),
        ("verified_untracked_paths", "verified.paths"),
        ("verified_untracked_stderr", "verified.stderr"),
        ("manifest", "manifest.json"),
    ];
    if files.len() != expected.len() {
        return Err(error("Incomplete registered artifact set"));
    }
    let mut total = 0_u64;
    for (kind, name) in expected {
        let path = format!("artifacts/{expected_id}/{name}");
        let mut matching = files.iter().filter(|file| file.kind == kind);
        let file = matching
            .next()
            .ok_or_else(|| error("Missing artifact kind"))?;
        if matching.next().is_some() || file.relative_path != path {
            return Err(error("Duplicate kind or unsafe artifact path"));
        }
        total = total
            .checked_add(file.size_bytes)
            .ok_or_else(|| error("Artifact size overflow"))?;
        let path = root.join(&file.relative_path);
        let metadata = private_file(&path)?;
        if metadata.mode() & 0o222 != 0 || metadata.len() != file.size_bytes {
            return Err(error("Artifact is writable or has an unexpected size"));
        }
        let actual = checksum(&path)?;
        if actual.size_bytes != file.size_bytes || actual.sha256 != file.sha256 {
            return Err(error("Registered artifact checksum mismatch"));
        }
    }
    let manifest_file = files
        .iter()
        .find(|file| file.kind == "manifest")
        .ok_or_else(|| error("Missing manifest"))?;
    let reader = BufReader::new(
        open(&root.join(&manifest_file.relative_path))?.take(manifest_file.size_bytes),
    );
    let manifest: RegisteredManifest = serde_json::from_reader(reader).map_err(error)?;
    let prepared = store
        .prepared_state(&run.prepared_state_id)
        .map_err(error)?
        .ok_or_else(|| error("Missing prepared state for artifact verification"))?;
    let capture = run
        .capture
        .as_ref()
        .ok_or_else(|| error("Missing capture metadata"))?;
    if manifest.format_version != 2
        || manifest.source_scope != "tracked_and_nonignored_untracked"
        || manifest.manifest_id != expected_id
        || manifest.run_id != run.run_id.as_str()
        || manifest.workspace_id != run.workspace_id.as_str()
        || manifest.prepared_state_id != run.prepared_state_id.as_str()
        || manifest.base_commit_sha != run.base_commit_sha
        || manifest.base_commit_sha != prepared.source_commit_sha
        || manifest.recipe_digest != prepared.recipe_digest
        || manifest.prompt_digest != run.prompt_digest
        || manifest.harness != run.harness
        || manifest.harness_version != run.harness_version
        || manifest.exit_code != run.exit_code
        || manifest.exit_signal != run.exit_signal
        || manifest.stop_reason.as_deref()
            != run
                .error_kind
                .as_deref()
                .filter(|reason| matches!(*reason, "timeout" | "cancelled"))
        || manifest.capture.limit_bytes != capture.limit_bytes
        || manifest.capture.persisted_bytes != capture.persisted_bytes
        || manifest.capture.truncated != capture.truncated
        || manifest.capture.stdout_bytes_seen != capture.stdout_bytes_seen
        || manifest.capture.stderr_bytes_seen != capture.stderr_bytes_seen
        || manifest.capture.stdout.size_bytes != capture.stdout.size_bytes
        || manifest.capture.stdout.sha256 != capture.stdout.sha256
        || manifest.capture.stderr.size_bytes != capture.stderr.size_bytes
        || manifest.capture.stderr.sha256 != capture.stderr.sha256
        || manifest.capture.events.size_bytes != capture.events.size_bytes
        || manifest.capture.events.sha256 != capture.events.sha256
        || manifest.capture.event_counts.parsed != capture.event_counts.parsed
        || manifest.capture.event_counts.invalid != capture.event_counts.invalid
        || manifest.capture.event_counts.oversized != capture.event_counts.oversized
        || total > manifest.artifact_limit_bytes
        || manifest.files.len() != files.len() - 1
    {
        return Err(error("Manifest metadata does not match registered run"));
    }
    let mut seen = HashSet::new();
    for entry in &manifest.files {
        if entry.kind == "manifest"
            || !seen.insert(entry.kind.as_str())
            || !files.iter().any(|file| {
                file.kind == entry.kind
                    && file.relative_path == entry.relative_path
                    && file.size_bytes == entry.size_bytes
                    && file.sha256 == entry.sha256
            })
        {
            return Err(error(
                "Manifest file entries do not match registered artifacts",
            ));
        }
    }
    for (kind, expected) in [
        ("stdout", &capture.stdout),
        ("stderr", &capture.stderr),
        ("events", &capture.events),
    ] {
        if !files.iter().any(|file| {
            file.kind == kind
                && file.size_bytes == expected.size_bytes
                && file.sha256 == expected.sha256
        }) {
            return Err(error("Capture output differs from registered artifacts"));
        }
    }
    if !files
        .iter()
        .any(|file| file.kind == "prompt" && file.sha256 == run.prompt_digest)
    {
        return Err(error("Prompt artifact differs from registered digest"));
    }
    let actual = checksum(&root.join(&manifest_file.relative_path))?;
    if actual.size_bytes != manifest_file.size_bytes || actual.sha256 != manifest_file.sha256 {
        return Err(error("Manifest changed during verification"));
    }
    Ok(())
}

pub(super) fn capture(
    root: &Path,
    store: &Store,
    id: &RunId,
    prepared: &PreparedState,
    output: &CaptureMetadata,
    limit: u64,
) -> Result<(ArtifactManifestId, Vec<Artifact>), FabricError> {
    let workspace = capture_workspace(root, id)?;
    let run = store
        .run(id)
        .map_err(error)?
        .ok_or_else(|| error("Missing Run"))?;
    let container = workspace
        .path()
        .parent()
        .ok_or_else(|| error("Missing container"))?;
    let manifest = ArtifactManifestId::try_from(format!("a-{id}"))?;
    let staging_root = root.join("artifact-staging");
    let final_root = root.join("artifacts");
    ensure_directory(&staging_root)?;
    ensure_directory(&final_root)?;
    sync(root)?;
    let stage = staging_root.join(manifest.as_str());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&stage)
        .map_err(error)?;
    sync(&staging_root)?;
    let mut remaining = limit;
    let mut files = Vec::with_capacity(12);
    for (name, kind, expected) in [
        ("stdout.log", "stdout", &output.stdout),
        ("stderr.log", "stderr", &output.stderr),
        ("events.jsonl", "events", &output.events),
    ] {
        let captured = copy(&container.join(name), &stage.join(name), &mut remaining)?;
        if &captured != expected {
            return Err(error("Output checksum changed before artifact capture"));
        }
        files.push(record(&manifest, name, kind, captured));
    }
    let prompt = copy(
        &container.join("prompt.input"),
        &stage.join("prompt.input"),
        &mut remaining,
    )?;
    if prompt.sha256 != run.prompt_digest {
        return Err(error("Prompt checksum changed before artifact capture"));
    }
    files.push(record(&manifest, "prompt.input", "prompt", prompt));

    validate_git(&workspace.path().join(".git"))?;
    for name in [
        ".git/commondir",
        ".git/objects/info/alternates",
        ".git/objects/info/http-alternates",
    ] {
        match fs::symlink_metadata(workspace.path().join(name)) {
            Ok(_) => return Err(error("Shared Git object storage is not capture-safe")),
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
            Err(failure) => return Err(error(failure)),
        }
    }
    // Reuse the owned, bounded process/drain path rather than buffering a binary patch.
    // Compare to the pinned launch HEAD, not a potentially worker-moved branch tip.
    let mut command = CommandSpec {
        program: "/usr/bin/git".into(),
        args: [
            "--git-dir=.git",
            "--work-tree=.",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "protocol.allow=never",
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "--ignore-submodules=none",
            &run.base_commit_sha,
            "--",
        ]
        .into_iter()
        .map(Into::into)
        .collect(),
        environment: [
            ("PATH", "/usr/bin:/bin"),
            ("LC_ALL", "C"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_OPTIONAL_LOCKS", "0"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect(),
    };
    let patch = capture_git(
        &command,
        workspace.path(),
        &stage,
        "tracked.patch",
        "git.stderr",
        &mut remaining,
    )?;
    files.push(record(
        &manifest,
        "tracked.patch",
        "tracked_patch",
        patch.stdout,
    ));
    files.push(record(
        &manifest,
        "git.stderr",
        "capture_diagnostic",
        patch.stderr,
    ));
    command.args.truncate(8);
    command.args.extend(
        ["ls-files", "--others", "--exclude-standard", "-z", "--"]
            .into_iter()
            .map(Into::into),
    );
    let listing = capture_git(
        &command,
        workspace.path(),
        &stage,
        "untracked.paths",
        "untracked.stderr",
        &mut remaining,
    )?;
    command.args.truncate(8);
    command.args.extend(
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
            "--",
        ]
        .into_iter()
        .map(Into::into),
    );
    let ignored = capture_git(
        &command,
        workspace.path(),
        &stage,
        "ignored.paths",
        "ignored.stderr",
        &mut remaining,
    )?;
    let archive = archive_untracked(
        workspace.path(),
        &stage.join("untracked.paths"),
        &stage.join("ignored.paths"),
        &stage.join("untracked.tar"),
        &mut remaining,
    )?;
    files.push(record(
        &manifest,
        "ignored.paths",
        "ignored_paths",
        ignored.stdout,
    ));
    files.push(record(
        &manifest,
        "ignored.stderr",
        "ignored_stderr",
        ignored.stderr,
    ));
    command.args.truncate(8);
    command.args.extend(
        ["ls-files", "--others", "--exclude-standard", "-z", "--"]
            .into_iter()
            .map(Into::into),
    );
    files.push(record(
        &manifest,
        "untracked.tar",
        "untracked_archive",
        archive,
    ));
    let verified = capture_git(
        &command,
        workspace.path(),
        &stage,
        "verified.paths",
        "verified.stderr",
        &mut remaining,
    )?;
    if listing.stdout != verified.stdout {
        return Err(error(
            "Untracked paths changed during capture; recovery required",
        ));
    }
    files.push(record(
        &manifest,
        "untracked.paths",
        "untracked_paths",
        listing.stdout,
    ));
    files.push(record(
        &manifest,
        "untracked.stderr",
        "untracked_stderr",
        listing.stderr,
    ));
    files.push(record(
        &manifest,
        "verified.paths",
        "verified_untracked_paths",
        verified.stdout,
    ));
    files.push(record(
        &manifest,
        "verified.stderr",
        "verified_untracked_stderr",
        verified.stderr,
    ));
    let metadata = Manifest {
        format_version: 2,
        manifest_id: manifest.as_str(),
        run_id: id.as_str(),
        workspace_id: run.workspace_id.as_str(),
        prepared_state_id: run.prepared_state_id.as_str(),
        base_commit_sha: &run.base_commit_sha,
        recipe_digest: &prepared.recipe_digest,
        prompt_digest: &run.prompt_digest,
        harness: &run.harness,
        harness_version: run.harness_version.as_deref(),
        exit_code: run.exit_code,
        exit_signal: run.exit_signal,
        stop_reason: run
            .error_kind
            .as_deref()
            .filter(|reason| matches!(*reason, "timeout" | "cancelled")),
        capture: output,
        artifact_limit_bytes: limit,
        source_scope: "tracked_and_nonignored_untracked",
        files: &files,
    };
    let bytes = serde_json::to_vec_pretty(&metadata).map_err(error)?;
    let mut file = create(&stage.join("manifest.json"))?;
    if bytes.len() as u64 > remaining {
        return Err(error(
            "Artifact limit exceeded by manifest; recovery required",
        ));
    }
    file.write_all(&bytes).map_err(error)?;
    file.sync_all().map_err(error)?;
    drop(file);
    files.push(record(
        &manifest,
        "manifest.json",
        "manifest",
        CapturedFile {
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        },
    ));
    for file in &files {
        let name = Path::new(&file.relative_path)
            .file_name()
            .ok_or_else(|| error("Invalid artifact name"))?;
        let path = stage.join(name);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).map_err(error)?;
        sync(&path)?;
    }
    sync(&stage)?;
    let destination = final_root.join(manifest.as_str());
    promote(&stage, &destination).map_err(|failure| {
        error(format!(
            "Cannot promote {}: {}",
            stage.display(),
            failure.message
        ))
    })?;
    // Darwin requires the source directory writable during rename; seal after promotion.
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o500)).map_err(error)?;
    sync(&destination)?;
    sync(&final_root)?;
    sync(&staging_root)?;
    for file in &files {
        let path = root.join(&file.relative_path);
        let actual = checksum(&path)?;
        if actual.size_bytes != file.size_bytes || actual.sha256 != file.sha256 {
            return Err(error("Promoted artifact checksum verification failed"));
        }
    }
    Ok((manifest, files))
}

fn capture_git(
    command: &CommandSpec,
    workspace: &Path,
    stage: &Path,
    stdout: &str,
    stderr: &str,
    remaining: &mut u64,
) -> Result<CaptureMetadata, FabricError> {
    let capture = fabric_runner::capture::Capture::new(
        create(&stage.join(stdout))?,
        create(&stage.join(stderr))?,
        None,
        *remaining,
    )
    .map_err(error)?;
    let result = fabric_runner::execute(
        command,
        workspace,
        capture,
        &ExecutionLimits {
            timeout: Duration::from_secs(60),
            ..Default::default()
        },
        &CancellationToken::default(),
        None,
        |_| Ok(()),
    )?;
    let capture = result.capture.map_err(error)?;
    if !result.status.success() || result.stop_reason.is_some() || capture.truncated {
        return Err(error(
            "Git artifact capture failed or exceeded artifact limit; recovery required",
        ));
    }
    *remaining = remaining
        .checked_sub(capture.persisted_bytes)
        .ok_or_else(|| error("Artifact limit exceeded"))?;
    Ok(capture)
}

// This writer counts tar headers, GNU long names/links, padding and end markers too.
struct ArchiveWriter<'a> {
    file: File,
    remaining: &'a mut u64,
    hasher: Sha256,
    size: u64,
}

impl Write for ArchiveWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > *self.remaining {
            return Err(std::io::Error::other(
                "Untracked archive exceeds artifact limit; recovery required",
            ));
        }
        let written = self.file.write(bytes)?;
        *self.remaining -= written as u64;
        self.size += written as u64;
        self.hasher.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn archive_untracked(
    workspace: &Path,
    listing: &Path,
    ignored_listing: &Path,
    destination: &Path,
    remaining: &mut u64,
) -> Result<CapturedFile, FabricError> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(workspace)
        .map_err(error)?;
    // Git omits FIFOs and sockets from ls-files entirely. Inspect nonignored directories
    // as well, so that their omission can never grant cleanup authority.
    let mut ignored = HashSet::new();
    for path in fs::read(ignored_listing)
        .map_err(error)?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = path.strip_suffix(b"/").unwrap_or(path);
        if !safe_path(path) {
            return Err(error("Unsafe ignored Git path"));
        }
        ignored.insert(PathBuf::from(OsStr::from_bytes(path)));
    }
    let mut inspected = Vec::new();
    inspect_directory(&root, Path::new(""), &ignored, &mut inspected)?;
    let writer = ArchiveWriter {
        file: create(destination)?,
        remaining,
        hasher: Sha256::new(),
        size: 0,
    };
    let mut archive = tar::Builder::new(writer);
    let mut directories = HashSet::new();
    let mut captured = Vec::new();
    let mut input = BufReader::new(open(listing)?);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if input.read_until(0, &mut bytes).map_err(error)? == 0 {
            break;
        }
        if bytes.pop() != Some(0) || !safe_path(&bytes) {
            return Err(error("Unsafe or unterminated untracked Git path"));
        }
        let path = Path::new(OsStr::from_bytes(&bytes));
        let mut prefix = PathBuf::new();
        for component in path.parent().unwrap().components() {
            prefix.push(component);
            if directories.insert(prefix.clone()) {
                let directory = open_relative(&root, &prefix)?;
                let metadata = stat(&directory)?;
                append_header(
                    &mut archive,
                    &prefix,
                    &metadata,
                    tar::EntryType::Directory,
                    std::io::empty(),
                )?;
                captured.push((prefix.clone(), metadata));
            }
        }
        let (parent, name, metadata) = entry(&root, path)?;
        let kind = metadata.st_mode & libc::S_IFMT;
        if metadata.st_uid != unsafe { libc::geteuid() } {
            return Err(error("Untracked input is not owned by the current user"));
        }
        if kind == libc::S_IFREG && metadata.st_nlink == 1 {
            let mut source = open_at(&parent, &name, libc::O_NONBLOCK)?;
            unchanged(&metadata, &stat(&source)?)?;
            append_header(
                &mut archive,
                path,
                &metadata,
                tar::EntryType::Regular,
                (&mut source).take(metadata.st_size as u64),
            )?;
            unchanged(&metadata, &stat(&source)?)?;
        } else if kind == libc::S_IFLNK {
            let mut target = [0u8; 4096];
            // SAFETY: valid parent descriptor, name and writable output buffer. No target is opened.
            let length = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if length < 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            let length = length as usize;
            if length == target.len() || !safe_link(&root, path, &target[..length])? {
                return Err(error("Unsafe untracked symlink target; recovery required"));
            }
            let mut header = header(&metadata, tar::EntryType::Symlink)?;
            archive
                .append_link(
                    &mut header,
                    path,
                    Path::new(OsStr::from_bytes(&target[..length])),
                )
                .map_err(error)?;
        } else {
            return Err(error(
                "Untracked input is not an independent file or safe symlink",
            ));
        }
        unchanged(&metadata, &entry(&root, path)?.2)?;
        captured.push((path.to_owned(), metadata));
    }
    // Reopen from the anchored root: changed parents, types, identities or content fail closed.
    for (path, metadata) in captured {
        unchanged(&metadata, &entry(&root, &path)?.2)?;
    }
    for (path, metadata) in inspected {
        if path.as_os_str().is_empty() {
            unchanged(&metadata, &stat(&root)?)?;
        } else {
            unchanged(&metadata, &entry(&root, &path)?.2)?;
        }
    }
    let writer = archive.into_inner().map_err(error)?;
    writer.file.sync_all().map_err(error)?;
    Ok(CapturedFile {
        size_bytes: writer.size,
        sha256: format!("{:x}", writer.hasher.finalize()),
    })
}

fn safe_path(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .split(|byte| *byte == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b".." && part != b".git")
}

fn safe_link(root: &File, path: &Path, bytes: &[u8]) -> Result<bool, FabricError> {
    if bytes.is_empty() {
        return Ok(false);
    }
    let mut target = path.parent().unwrap().to_owned();
    // Do not normalize "..": an earlier component might be a symlink.
    for component in Path::new(OsStr::from_bytes(bytes)).components() {
        match component {
            Component::Normal(name) if name != ".git" => target.push(name),
            Component::CurDir => (),
            _ => return Ok(false),
        }
    }
    // Refuse chains and symlink ancestors, rather than resolving (and reading) outside paths.
    if target.as_os_str().is_empty() {
        return Ok(true);
    }
    Ok(entry(root, &target)?.2.st_mode & libc::S_IFMT != libc::S_IFLNK)
}

fn inspect_directory(
    directory: &File,
    prefix: &Path,
    ignored: &HashSet<PathBuf>,
    inspected: &mut Vec<(PathBuf, libc::stat)>,
) -> Result<(), FabricError> {
    inspected.push((prefix.to_owned(), stat(directory)?));
    // fdopendir owns this duplicate, leaving our anchored descriptor alive.
    // SAFETY: directory is a live descriptor.
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    // SAFETY: duplicate transfers to the directory stream on success.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        // SAFETY: fdopendir failed, so duplicate is still ours.
        unsafe {
            libc::close(duplicate);
        }
        return Err(error(std::io::Error::last_os_error()));
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: this owns the live directory stream.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    loop {
        #[cfg(target_os = "macos")]
        // SAFETY: libc supplies this thread's errno location.
        let errno = unsafe { libc::__error() };
        #[cfg(not(target_os = "macos"))]
        // SAFETY: libc supplies this thread's errno location.
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: errno is thread-local and the directory stream is live.
        unsafe {
            *errno = 0;
        }
        let item = unsafe { libc::readdir(stream.0) };
        if item.is_null() {
            // SAFETY: errno is a valid thread-local pointer.
            if unsafe { *errno } != 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            break;
        }
        // SAFETY: readdir's name is NUL-terminated and valid until the next call.
        let name = unsafe { CStr::from_ptr((*item).d_name.as_ptr()) };
        if [b".".as_slice(), b"..", b".git"].contains(&name.to_bytes()) {
            continue;
        }
        let path = prefix.join(OsStr::from_bytes(name.to_bytes()));
        if ignored.contains(&path) {
            continue;
        }
        let metadata = stat_at(directory, name)?;
        match metadata.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child = open_at(directory, name, libc::O_DIRECTORY)?;
                unchanged(&metadata, &stat(&child)?)?;
                inspect_directory(&child, &path, ignored, inspected)?;
                unchanged(&metadata, &stat_at(directory, name)?)?;
            }
            libc::S_IFREG | libc::S_IFLNK => (),
            _ => {
                return Err(error(
                    "Nonignored special file cannot be safely archived; recovery required",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn open_at(parent: &File, name: &CStr, flags: i32) -> Result<File, FabricError> {
    // SAFETY: valid descriptor and NUL-terminated component; no symlink is followed.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | flags,
        )
    };
    if fd < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned a new, exclusively owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_relative(root: &File, path: &Path) -> Result<File, FabricError> {
    let mut directory = root.try_clone().map_err(error)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(error("Unsafe archive ancestor"));
        };
        directory = open_at(
            &directory,
            &CString::new(name.as_bytes()).map_err(error)?,
            libc::O_DIRECTORY,
        )?;
    }
    Ok(directory)
}

fn entry(root: &File, path: &Path) -> Result<(File, CString, libc::stat), FabricError> {
    let parent = open_relative(
        root,
        path.parent()
            .ok_or_else(|| error("Missing archive parent"))?,
    )?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| error("Missing archive name"))?
            .as_bytes(),
    )
    .map_err(error)?;
    let metadata = stat_at(&parent, &name)?;
    Ok((parent, name, metadata))
}

pub(super) fn stat_at(parent: &File, name: &CStr) -> Result<libc::stat, FabricError> {
    let mut metadata = std::mem::MaybeUninit::uninit();
    // SAFETY: valid descriptor, component and output storage; final symlinks are not followed.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(error(std::io::Error::last_os_error()));
    }
    // SAFETY: successful fstatat initialized the output.
    Ok(unsafe { metadata.assume_init() })
}

pub(super) fn stat(file: &File) -> Result<libc::stat, FabricError> {
    let mut metadata = std::mem::MaybeUninit::uninit();
    // SAFETY: valid descriptor and output storage.
    if unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    // SAFETY: successful fstat initialized the output.
    Ok(unsafe { metadata.assume_init() })
}

fn unchanged(before: &libc::stat, after: &libc::stat) -> Result<(), FabricError> {
    if before.st_dev != after.st_dev
        || before.st_ino != after.st_ino
        || before.st_mode != after.st_mode
        || before.st_nlink != after.st_nlink
        || before.st_uid != after.st_uid
        || before.st_gid != after.st_gid
        || before.st_size != after.st_size
        || before.st_mtime != after.st_mtime
        || before.st_mtime_nsec != after.st_mtime_nsec
        || before.st_ctime != after.st_ctime
        || before.st_ctime_nsec != after.st_ctime_nsec
    {
        return Err(error(
            "Untracked input changed during capture; recovery required",
        ));
    }
    Ok(())
}

fn header(metadata: &libc::stat, kind: tar::EntryType) -> Result<tar::Header, FabricError> {
    if metadata.st_mode & 0o7000 != 0 || metadata.st_size < 0 || metadata.st_mtime < 0 {
        return Err(error("Untracked metadata cannot be safely represented"));
    }
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(u32::from(metadata.st_mode) & 0o777);
    header.set_uid(u64::from(metadata.st_uid));
    header.set_gid(u64::from(metadata.st_gid));
    header.set_mtime(metadata.st_mtime as u64);
    header.set_size(if kind.is_file() {
        metadata.st_size as u64
    } else {
        0
    });
    Ok(header)
}

fn append_header(
    archive: &mut tar::Builder<ArchiveWriter<'_>>,
    path: &Path,
    metadata: &libc::stat,
    kind: tar::EntryType,
    source: impl Read,
) -> Result<(), FabricError> {
    archive
        .append_data(&mut header(metadata, kind)?, path, source)
        .map_err(error)
}

fn record(manifest: &ArtifactManifestId, name: &str, kind: &str, file: CapturedFile) -> Artifact {
    Artifact {
        kind: kind.into(),
        relative_path: format!("artifacts/{manifest}/{name}"),
        sha256: file.sha256,
        size_bytes: file.size_bytes,
    }
}

fn ensure_directory(path: &Path) -> Result<(), FabricError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(failure) => return Err(error(failure)),
    }
    private_directory(path)
}

fn create(path: &Path) -> Result<File, FabricError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(error)
}

fn open(path: &Path) -> Result<File, FabricError> {
    let recorded = private_file(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(error)?;
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.ino() != recorded.ino()
        || metadata.dev() != recorded.dev()
        || metadata.uid() != recorded.uid()
        || metadata.mode() != recorded.mode()
    {
        return Err(error("Artifact input changed during open"));
    }
    Ok(file)
}

fn copy(
    source: &Path,
    destination: &Path,
    remaining: &mut u64,
) -> Result<CapturedFile, FabricError> {
    let mut source = open(source)?;
    let mut target = create(destination)?;
    let mut hasher = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = source.read(&mut buffer).map_err(error)?;
        if read == 0 {
            break;
        }
        *remaining = remaining
            .checked_sub(read as u64)
            .ok_or_else(|| error("Artifact limit exceeded; recovery required"))?;
        target.write_all(&buffer[..read]).map_err(error)?;
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    target.sync_all().map_err(error)?;
    Ok(CapturedFile {
        size_bytes: size,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn checksum(path: &Path) -> Result<CapturedFile, FabricError> {
    let mut file = open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(CapturedFile {
        size_bytes: size,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn validate_git(path: &Path) -> Result<(), FabricError> {
    let meta = fs::symlink_metadata(path).map_err(error)?;
    // The container is owner-only; Git's own files may retain ordinary modes.
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } || meta.file_type().is_symlink() {
        return Err(error("Git metadata must be independently owned real files"));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(error)? {
            validate_git(&entry.map_err(error)?.path())?;
        }
    } else if !meta.is_file() || meta.nlink() != 1 {
        return Err(error("Git metadata is not an independent regular file"));
    }
    Ok(())
}

fn sync(path: &Path) -> Result<(), FabricError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|failure| error(format!("Cannot sync {}: {failure}", path.display())))
}

#[cfg(target_os = "macos")]
fn promote(stage: &Path, destination: &Path) -> Result<(), FabricError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let stage = CString::new(stage.as_os_str().as_bytes()).map_err(error)?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(error)?;
    // SAFETY: valid NUL-terminated paths; RENAME_EXCL refuses any existing destination.
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(error(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn promote(_stage: &Path, _destination: &Path) -> Result<(), FabricError> {
    Err(error(
        "Atomic artifact publication currently supports macOS only",
    ))
}

fn error(value: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: format!("Artifact capture: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn untracked_paths_and_descriptor_validation_refuse_tampering() {
        assert!(safe_path(b"literal\nname/\xff"));
        for path in [
            b"/outside".as_slice(),
            b"../outside",
            b"a/../outside",
            b"a//b",
            b".git/config",
            b"a/./b",
        ] {
            assert!(!safe_path(path));
        }
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(tree.join("dir")).unwrap();
        fs::write(tree.join("dir/file"), b"original").unwrap();
        let root = File::open(&tree).unwrap();
        fs::write(tree.join("inside"), b"lexical bait").unwrap();
        symlink(temp.path(), tree.join("hop")).unwrap();
        assert!(!safe_link(&root, Path::new("link"), b"hop/../inside").unwrap());
        let before = entry(&root, Path::new("dir/file")).unwrap().2;
        fs::remove_file(tree.join("dir/file")).unwrap();
        symlink("../../outside", tree.join("dir/file")).unwrap();
        let (parent, name, after) = entry(&root, Path::new("dir/file")).unwrap();
        assert!(unchanged(&before, &after).is_err());
        assert!(open_at(&parent, &name, libc::O_NONBLOCK).is_err());
        fs::write(temp.path().join("outside"), b"outside sentinel").unwrap();
        fs::remove_file(tree.join("dir/file")).unwrap();
        fs::remove_dir(tree.join("dir")).unwrap();
        symlink(temp.path(), tree.join("dir")).unwrap();
        assert!(entry(&root, Path::new("dir/outside")).is_err());
        let listing = temp.path().join("paths");
        fs::write(&listing, b"dir/outside\0").unwrap();
        fs::set_permissions(&listing, fs::Permissions::from_mode(0o600)).unwrap();
        let ignored = temp.path().join("ignored");
        fs::write(&ignored, []).unwrap();
        let mut remaining = 65536;
        assert!(
            archive_untracked(
                &tree,
                &listing,
                &ignored,
                &temp.path().join("archive.tar"),
                &mut remaining
            )
            .is_err()
        );
        assert_eq!(
            fs::read(temp.path().join("outside")).unwrap(),
            b"outside sentinel"
        );
    }
}
