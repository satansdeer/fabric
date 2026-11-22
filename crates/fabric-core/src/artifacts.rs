//! Tracked recovery evidence. Publication never authorizes workspace deletion.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use fabric_domain::{
    ArtifactManifestId, CaptureMetadata, CapturedFile, ErrorKind, FabricError, RunId,
};
use fabric_runner::{CancellationToken, CommandSpec, ExecutionLimits};
use fabric_store::{Artifact, PreparedState, Store};
use serde::Serialize;
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
    // This manifest covers tracked edits only, even when the workspace has no untracked files.
    source_scope: &'static str,
    files: &'a [Artifact],
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
    let mut files = Vec::with_capacity(7);
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
    let git_capture = fabric_runner::capture::Capture::new(
        create(&stage.join("tracked.patch"))?,
        create(&stage.join("git.stderr"))?,
        None,
        remaining,
    )
    .map_err(error)?;
    let command = CommandSpec {
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
    let result = fabric_runner::execute(
        &command,
        workspace.path(),
        git_capture,
        &ExecutionLimits {
            timeout: Duration::from_secs(60),
            ..Default::default()
        },
        &CancellationToken::default(),
        |_| Ok(()),
    )?;
    let patch = result.capture.map_err(error)?;
    if !result.status.success() || result.stop_reason.is_some() || patch.truncated {
        return Err(error(
            "Tracked artifact capture failed or exceeded artifact limit; recovery required",
        ));
    }
    remaining = remaining
        .checked_sub(patch.persisted_bytes)
        .ok_or_else(|| error("Artifact limit exceeded"))?;
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
    let metadata = Manifest {
        format_version: 1,
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
        source_scope: "tracked_only",
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
