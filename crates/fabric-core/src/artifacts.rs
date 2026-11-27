//! Complete source recovery evidence; deletion requires separate GC checks.
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use fabric_domain::{
    ArtifactManifestId, CaptureMetadata, CapturedFile, ErrorKind, FabricError, ProfileEvidence,
    RunId,
};
use fabric_runner::{CancellationToken, CommandSpec, ExecutionLimits};
use fabric_store::{Artifact, PreparedState, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workspace::{capture_workspace, private_directory, private_file};

// Directory captures use the same inventory, but only the archive contains source evidence.
const DIRECTORY_EMPTY_FILES: [(&str, &str); 8] = [
    ("tracked.patch", "tracked_patch"),
    ("git.stderr", "capture_diagnostic"),
    ("ignored.paths", "ignored_paths"),
    ("ignored.stderr", "ignored_stderr"),
    ("untracked.paths", "untracked_paths"),
    ("untracked.stderr", "untracked_stderr"),
    ("verified.paths", "verified_untracked_paths"),
    ("verified.stderr", "verified_untracked_stderr"),
];

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
    profile: Option<&'a ProfileEvidence>,
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
    #[serde(default)]
    profile: Option<ProfileEvidence>,
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
    #[serde(default)]
    normalized_event_count: u64,
    #[serde(default)]
    native_failure: Option<String>,
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

/// Validate retained sealed recovery evidence and return the exact verified inventory.
pub(super) fn verify_registered(
    root: &Path,
    store: &Store,
    run: &fabric_store::Run,
) -> Result<Vec<Artifact>, FabricError> {
    let manifest = run
        .artifact_manifest_id
        .as_ref()
        .ok_or_else(|| error("Missing registered artifact manifest"))?;
    let files = store.artifacts(&run.run_id).map_err(error)?;
    let directory = root.join("artifacts").join(format!("a-{}", run.run_id));
    verify_files(root, store, run, manifest, &directory, &files, true)?;
    Ok(files)
}

fn verify_files(
    root: &Path,
    store: &Store,
    run: &fabric_store::Run,
    manifest: &ArtifactManifestId,
    directory: &Path,
    files: &[Artifact],
    sealed: bool,
) -> Result<(), FabricError> {
    let expected_id = format!("a-{}", run.run_id);
    if manifest.as_str() != expected_id {
        return Err(error("Missing or unexpected registered artifact manifest"));
    }
    private_directory(root)?;
    if fs::canonicalize(root).map_err(error)? != root {
        return Err(error("Artifact root is not canonical"));
    }
    let parent = directory
        .parent()
        .ok_or_else(|| error("Missing artifact parent"))?;
    for path in [parent, directory] {
        private_directory(path)?;
        if fs::canonicalize(path).map_err(error)? != *path {
            return Err(error("Artifact directory has a symlink ancestor"));
        }
    }
    if sealed && fs::symlink_metadata(directory).map_err(error)?.mode() & 0o222 != 0 {
        return Err(error("Artifact directory is not sealed"));
    }
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
    if files.len() != expected.len()
        || fs::read_dir(directory).map_err(error)?.count() != expected.len()
    {
        return Err(error("Incomplete or foreign artifact set"));
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
        let path = directory.join(name);
        let metadata = private_file(&path)?;
        if (sealed && metadata.mode() & 0o222 != 0) || metadata.len() != file.size_bytes {
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
    let reader =
        BufReader::new(open(&directory.join("manifest.json"))?.take(manifest_file.size_bytes));
    let manifest: RegisteredManifest = serde_json::from_reader(reader).map_err(error)?;
    if let Some(context) = store.capture_context(&run.run_id).map_err(error)? {
        if manifest.artifact_limit_bytes != context.artifact_limit_bytes
            || serde_json::to_value(&manifest.profile).map_err(error)?
                != serde_json::to_value(Some(&context.profile)).map_err(error)?
        {
            return Err(error("Manifest differs from immutable capture context"));
        }
    }
    let attempts = store.worker_attempts(&run.run_id).map_err(error)?;
    let profile_digest = manifest
        .profile
        .as_ref()
        .map(|profile| profile.digest.as_str());
    if profile_digest.is_some_and(|digest| {
        digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) || (profile_digest.is_some() && attempts.is_empty())
        || attempts
            .iter()
            .any(|attempt| attempt.profile_digest.as_deref() != profile_digest)
    {
        return Err(error(
            "Manifest profile does not match the registered worker attempt",
        ));
    }
    let prepared = store
        .prepared_state(&run.prepared_state_id)
        .map_err(error)?
        .ok_or_else(|| error("Missing prepared state for artifact verification"))?;
    let capture = run
        .capture
        .as_ref()
        .ok_or_else(|| error("Missing capture metadata"))?;
    let directory_source = fabric_project::is_directory_source(&prepared.source_commit_sha);
    let source_scope = if directory_source {
        "directory_full_tree"
    } else {
        "tracked_and_nonignored_untracked"
    };
    if manifest.format_version != 2
        || manifest.source_scope != source_scope
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
                .filter(|reason| matches!(*reason, "timeout" | "cancelled" | "native_failure"))
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
        || manifest.capture.normalized_event_count != capture.normalized_event_count
        || manifest.capture.native_failure != capture.native_failure
        || total > manifest.artifact_limit_bytes
        || manifest.files.len() != files.len() - 1
    {
        return Err(error("Manifest metadata does not match registered run"));
    }
    if directory_source
        && DIRECTORY_EMPTY_FILES.iter().any(|(_, kind)| {
            files
                .iter()
                .any(|file| file.kind == *kind && file.size_bytes != 0)
        })
    {
        return Err(error("Directory capture contains Git-only evidence"));
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
    let actual = checksum(&directory.join("manifest.json"))?;
    if actual.size_bytes != manifest_file.size_bytes || actual.sha256 != manifest_file.sha256 {
        return Err(error("Manifest changed during verification"));
    }
    Ok(())
}

/// The caller holds root admission/cleanup authority and a proven inactive owner.
/// Existing partial or foreign captures are never overwritten or removed.
pub(super) fn recover_capture(
    root: &Path,
    store: &Store,
    run: &fabric_store::Run,
    workspace: &crate::workspace::RunnerWorkspace,
    apply: bool,
) -> Result<Option<(ArtifactManifestId, Vec<Artifact>)>, FabricError> {
    let manifest = ArtifactManifestId::try_from(format!("a-{}", run.run_id))?;
    let destination = root.join("artifacts").join(manifest.as_str());
    let stage = root.join("artifact-staging").join(manifest.as_str());
    let present = |path: &Path| match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(failure) => Err(error(failure)),
    };
    let final_present = present(&destination)?;
    let stage_present = present(&stage)?;
    if final_present && stage_present {
        return Err(error(
            "Both staged and promoted capture evidence exist; retain both for inspection",
        ));
    }
    if final_present || stage_present {
        let directory = if final_present { &destination } else { &stage };
        private_directory(directory)?;
        if fs::canonicalize(directory).map_err(error)? != *directory {
            return Err(error("Capture evidence path has a symlink ancestor"));
        }
        let manifest_path = directory.join("manifest.json");
        let metadata = private_file(&manifest_path)?;
        // A manifest has a fixed fourteen-file vocabulary; bound untrusted JSON.
        if metadata.len() > 16 * 1024 * 1024 {
            return Err(error("Capture manifest exceeds inspection bound"));
        }
        let mut bytes = Vec::new();
        open(&manifest_path)?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(error("Capture manifest changed during inspection"));
        }
        let metadata: RegisteredManifest = serde_json::from_slice(&bytes).map_err(|failure| {
            error(format!(
                "Incomplete or invalid capture manifest; retain evidence: {failure}"
            ))
        })?;
        let mut files = metadata
            .files
            .into_iter()
            .map(|file| Artifact {
                kind: file.kind,
                relative_path: file.relative_path,
                sha256: file.sha256,
                size_bytes: file.size_bytes,
            })
            .collect::<Vec<_>>();
        files.push(Artifact {
            kind: "manifest".into(),
            relative_path: format!("artifacts/{manifest}/manifest.json"),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
        });
        verify_files(root, store, run, &manifest, directory, &files, false)?;
        if apply {
            for file in &files {
                let name = Path::new(&file.relative_path)
                    .file_name()
                    .ok_or_else(|| error("Invalid artifact name"))?;
                let path = directory.join(name);
                fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).map_err(error)?;
                sync(&path)?;
            }
            sync(directory)?;
            if stage_present {
                ensure_directory(&root.join("artifacts"))?;
                promote(&stage, &destination)?;
            }
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o500)).map_err(error)?;
            sync(&destination)?;
            sync(&root.join("artifacts"))?;
            if stage_present {
                sync(&root.join("artifact-staging"))?;
            }
            verify_files(root, store, run, &manifest, &destination, &files, true)?;
        }
        return Ok(Some((manifest, files)));
    }
    let output = run
        .capture
        .as_ref()
        .ok_or_else(|| error("Complete durable capture metadata is unavailable; unknown output and completion remain retained"))?;
    let context = store
        .capture_context(&run.run_id)
        .map_err(error)?
        .ok_or_else(|| error("Immutable profile and artifact-limit capture context is unavailable; historical evidence cannot be reconstructed"))?;
    let attempts = store.worker_attempts(&run.run_id).map_err(error)?;
    if attempts.is_empty()
        || attempts.iter().any(|attempt| {
            attempt.profile_digest.as_deref() != Some(context.profile.digest.as_str())
        })
    {
        return Err(error(
            "Capture context differs from durable worker profile evidence",
        ));
    }
    let container = workspace
        .path()
        .parent()
        .ok_or_else(|| error("Missing container"))?;
    for (name, expected) in [
        ("stdout.log", &output.stdout),
        ("stderr.log", &output.stderr),
        ("events.jsonl", &output.events),
    ] {
        if &checksum(&container.join(name))? != expected {
            return Err(error(
                "Durable output checksum changed; recovery evidence retained",
            ));
        }
    }
    if checksum(&container.join("prompt.input"))?.sha256 != run.prompt_digest {
        return Err(error(
            "Recovery prompt differs from durable admission digest",
        ));
    }
    let prepared = store
        .prepared_state(&run.prepared_state_id)
        .map_err(error)?
        .ok_or_else(|| error("Missing pinned prepared state"))?;
    if !fabric_project::is_directory_source(&prepared.source_commit_sha) {
        validate_git(&workspace.path().join(".git"))?;
    }
    if !apply {
        return Ok(None);
    }
    let captured = capture_in_workspace(
        root,
        store,
        workspace,
        &prepared,
        output,
        &context.profile,
        context.artifact_limit_bytes,
    )?;
    verify_files(
        root,
        store,
        run,
        &captured.0,
        &destination,
        &captured.1,
        true,
    )?;
    Ok(Some(captured))
}

pub(super) fn capture(
    root: &Path,
    store: &Store,
    id: &RunId,
    prepared: &PreparedState,
    output: &CaptureMetadata,
    profile: &ProfileEvidence,
    limit: u64,
) -> Result<(ArtifactManifestId, Vec<Artifact>), FabricError> {
    let workspace = capture_workspace(root, id)?;
    capture_in_workspace(root, store, &workspace, prepared, output, profile, limit)
}

fn capture_in_workspace(
    root: &Path,
    store: &Store,
    workspace: &crate::workspace::RunnerWorkspace,
    prepared: &PreparedState,
    output: &CaptureMetadata,
    profile: &ProfileEvidence,
    limit: u64,
) -> Result<(ArtifactManifestId, Vec<Artifact>), FabricError> {
    let id = workspace.run_id();
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

    let directory_source = fabric_project::is_directory_source(&prepared.source_commit_sha);
    if directory_source != fabric_project::is_directory_source(&run.base_commit_sha) {
        return Err(error("Source kind differs from the pinned run"));
    }
    if directory_source {
        let empty_sha256 = format!("{:x}", Sha256::digest([]));
        for (name, kind) in DIRECTORY_EMPTY_FILES {
            create(&stage.join(name))?.sync_all().map_err(error)?;
            files.push(record(
                &manifest,
                name,
                kind,
                CapturedFile {
                    size_bytes: 0,
                    sha256: empty_sha256.clone(),
                },
            ));
        }
        let archive = archive_source(
            workspace.path(),
            None,
            &stage.join("untracked.tar"),
            &mut remaining,
        )?;
        files.push(record(
            &manifest,
            "untracked.tar",
            "untracked_archive",
            archive,
        ));
    } else {
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
        // Even with optional locks disabled, Git diff can refresh the index.
        // Capture must preserve the returned tree's immutable installation digest.
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
                "diff.autoRefreshIndex=false",
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
        command.args.truncate(10);
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
        command.args.truncate(10);
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
        let archive = archive_source(
            workspace.path(),
            Some((&stage.join("untracked.paths"), &stage.join("ignored.paths"))),
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
        command.args.truncate(10);
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
    }
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
        profile: Some(profile),
        exit_code: run.exit_code,
        exit_signal: run.exit_signal,
        stop_reason: run
            .error_kind
            .as_deref()
            .filter(|reason| matches!(*reason, "timeout" | "cancelled" | "native_failure")),
        capture: output,
        artifact_limit_bytes: limit,
        source_scope: if directory_source {
            "directory_full_tree"
        } else {
            "tracked_and_nonignored_untracked"
        },
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

fn archive_source(
    workspace: &Path,
    listing: Option<(&Path, &Path)>,
    destination: &Path,
    remaining: &mut u64,
) -> Result<CapturedFile, FabricError> {
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(workspace)
        .map_err(error)?;
    // Git omits special files from ls-files. Directory mode inspects every entry,
    // including hidden files and empty directories, without Git ignore semantics.
    let full_tree = listing.is_none();
    let mut ignored = HashSet::new();
    if let Some((_, ignored_listing)) = listing {
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
    }
    let mut inspected = Vec::new();
    inspect_directory(&root, Path::new(""), &ignored, full_tree, &mut inspected)?;
    let writer = ArchiveWriter {
        file: create(destination)?,
        remaining,
        hasher: Sha256::new(),
        size: 0,
    };
    let mut archive = tar::Builder::new(writer);
    let mut directories = HashSet::new();
    let mut captured = Vec::new();
    let mut input = listing
        .map(|(listing, _)| open(listing).map(BufReader::new))
        .transpose()?;
    let mut full_paths = inspected
        .iter()
        .filter(|(path, _)| !path.as_os_str().is_empty());
    let mut bytes = Vec::new();
    loop {
        let path = if let Some(input) = &mut input {
            bytes.clear();
            if input.read_until(0, &mut bytes).map_err(error)? == 0 {
                break;
            }
            if bytes.pop() != Some(0) || !safe_path(&bytes) {
                return Err(error("Unsafe or unterminated untracked Git path"));
            }
            Path::new(OsStr::from_bytes(&bytes))
        } else if let Some((path, _)) = full_paths.next() {
            path.as_path()
        } else {
            break;
        };
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
            return Err(error("Archive input is not owned by the current user"));
        }
        if full_tree && kind == libc::S_IFDIR {
            if directories.insert(path.to_owned()) {
                append_header(
                    &mut archive,
                    path,
                    &metadata,
                    tar::EntryType::Directory,
                    std::io::empty(),
                )?;
            }
        } else if kind == libc::S_IFREG && metadata.st_nlink == 1 {
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
            if length == target.len() || !safe_link(&root, path, &target[..length], full_tree)? {
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
    if full_tree {
        let current_root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(workspace)
            .map_err(error)?;
        unchanged(&stat(&root)?, &stat(&current_root)?)?;
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

fn safe_link(root: &File, path: &Path, bytes: &[u8], full_tree: bool) -> Result<bool, FabricError> {
    if bytes.is_empty() {
        return Ok(false);
    }
    if full_tree {
        fabric_project::prepare::validate_symlink_at(
            root,
            path,
            Instant::now() + Duration::from_secs(60),
        )
        .map_err(error)?;
        return Ok(true);
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
    full_tree: bool,
    inspected: &mut Vec<(PathBuf, libc::stat)>,
) -> Result<(), FabricError> {
    let metadata = stat(directory)?;
    if full_tree && metadata.st_uid != unsafe { libc::geteuid() } {
        return Err(error("Archive directory is not owned by the current user"));
    }
    inspected.push((prefix.to_owned(), metadata));
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
        if [b".".as_slice(), b".."].contains(&name.to_bytes())
            || (!full_tree && name.to_bytes() == b".git")
        {
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
                inspect_directory(&child, &path, ignored, full_tree, inspected)?;
                unchanged(&metadata, &stat_at(directory, name)?)?;
            }
            libc::S_IFREG | libc::S_IFLNK => {
                if full_tree {
                    inspected.push((path, metadata));
                }
            }
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

pub(super) fn open(path: &Path) -> Result<File, FabricError> {
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
        assert!(!safe_link(&root, Path::new("link"), b"hop/../inside", false).unwrap());
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
            archive_source(
                &tree,
                Some((&listing, &ignored)),
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

    #[test]
    fn directory_archive_round_trips_the_complete_final_tree_without_git() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        let empty_archive = temp.path().join("empty.tar");
        let mut remaining = 65536;
        let empty = archive_source(&tree, None, &empty_archive, &mut remaining).unwrap();
        assert_eq!(empty.size_bytes, 1024);
        assert_eq!(
            tar::Archive::new(File::open(empty_archive).unwrap())
                .entries()
                .unwrap()
                .count(),
            0
        );
        fs::write(tree.join("deleted"), b"old source").unwrap();
        fs::remove_file(tree.join("deleted")).unwrap();
        fs::write(tree.join("binary"), b"\0\xff\n\x80").unwrap();
        fs::set_permissions(tree.join("binary"), fs::Permissions::from_mode(0o751)).unwrap();
        fs::write(tree.join("new"), b"worker output").unwrap();
        fs::write(tree.join(".gitignore"), b".hidden\n").unwrap();
        fs::write(tree.join(".hidden"), b"not ignored in directory mode").unwrap();
        fs::create_dir_all(tree.join("nested/empty")).unwrap();
        fs::create_dir(tree.join(".git")).unwrap();
        fs::write(tree.join(".git/ordinary"), b"worker-created ordinary data").unwrap();
        // APFS requires UTF-8 names; Unix filesystems without that restriction also cover raw bytes.
        #[cfg(target_os = "macos")]
        let unusual = OsStr::new("literal\n字");
        #[cfg(not(target_os = "macos"))]
        let unusual = OsStr::from_bytes(b"literal\n\xff");
        fs::write(tree.join(unusual), b"unusual filename").unwrap();
        symlink("binary", tree.join("link")).unwrap();
        symlink(".git/ordinary", tree.join("metadata-link")).unwrap();
        symlink("../binary", tree.join("nested/parent-link")).unwrap();
        symlink("link", tree.join("chain")).unwrap();

        let destination = temp.path().join("tree.tar");
        let budget = remaining;
        let captured = archive_source(&tree, None, &destination, &mut remaining).unwrap();
        assert_eq!(budget - remaining, captured.size_bytes);
        assert_eq!(checksum(&destination).unwrap(), captured);
        let mut archive = tar::Archive::new(File::open(&destination).unwrap());
        let paths = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<HashSet<_>>();
        let expected = [
            "binary",
            "new",
            ".gitignore",
            ".hidden",
            "nested",
            "nested/empty",
            ".git",
            ".git/ordinary",
            "link",
            "metadata-link",
            "nested/parent-link",
            "chain",
        ]
        .into_iter()
        .map(PathBuf::from)
        .chain([PathBuf::from(unusual)])
        .collect::<HashSet<_>>();
        assert_eq!(paths, expected);
        let restored = temp.path().join("restored");
        let mut archive = tar::Archive::new(File::open(&destination).unwrap());
        archive.set_preserve_permissions(true);
        archive.unpack(&restored).unwrap();
        for name in ["binary", "new", ".gitignore", ".hidden", ".git/ordinary"] {
            assert_eq!(
                fs::read(restored.join(name)).unwrap(),
                fs::read(tree.join(name)).unwrap()
            );
        }
        assert_eq!(
            fs::read(restored.join(unusual)).unwrap(),
            b"unusual filename"
        );
        assert_eq!(
            fs::metadata(restored.join("binary")).unwrap().mode() & 0o777,
            0o751
        );
        assert!(restored.join("nested/empty").is_dir());
        assert!(!restored.join("deleted").exists());
        assert_eq!(
            fs::read_link(restored.join("link")).unwrap(),
            Path::new("binary")
        );
        assert_eq!(
            fs::read_link(restored.join("metadata-link")).unwrap(),
            Path::new(".git/ordinary")
        );
        assert_eq!(
            fs::read_link(restored.join("nested/parent-link")).unwrap(),
            Path::new("../binary")
        );
        assert_eq!(
            fs::read_link(restored.join("chain")).unwrap(),
            Path::new("link")
        );
        assert_eq!(
            fs::read(restored.join("nested/parent-link")).unwrap(),
            b"\0\xff\n\x80"
        );
        assert_eq!(fs::read(restored.join("chain")).unwrap(), b"\0\xff\n\x80");
    }

    #[test]
    fn directory_archive_failures_preserve_the_source_tree() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("source"), b"retained").unwrap();
        let mut remaining = 1;
        assert!(
            archive_source(
                &tree,
                None,
                &temp.path().join("over-budget.tar"),
                &mut remaining
            )
            .is_err()
        );
        symlink(temp.path(), tree.join("escape")).unwrap();
        remaining = 65536;
        assert!(
            archive_source(
                &tree,
                None,
                &temp.path().join("unsafe-link.tar"),
                &mut remaining
            )
            .is_err()
        );
        fs::remove_file(tree.join("escape")).unwrap();
        fs::hard_link(tree.join("source"), tree.join("alias")).unwrap();
        remaining = 65536;
        assert!(
            archive_source(
                &tree,
                None,
                &temp.path().join("hardlink.tar"),
                &mut remaining
            )
            .is_err()
        );
        fs::remove_file(tree.join("alias")).unwrap();
        symlink("cycle-b", tree.join("cycle-a")).unwrap();
        symlink("cycle-a", tree.join("cycle-b")).unwrap();
        remaining = 65536;
        assert!(
            archive_source(&tree, None, &temp.path().join("cycle.tar"), &mut remaining).is_err()
        );
        fs::remove_file(tree.join("cycle-a")).unwrap();
        fs::remove_file(tree.join("cycle-b")).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(tree.join("socket")).unwrap();
        remaining = 65536;
        assert!(
            archive_source(
                &tree,
                None,
                &temp.path().join("special.tar"),
                &mut remaining
            )
            .is_err()
        );
        drop(socket);
        assert_eq!(fs::read(tree.join("source")).unwrap(), b"retained");
        assert!(tree.join("socket").exists());
    }

    #[test]
    fn directory_recovery_recaptures_without_git_and_reclaims_verified_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("gone"), b"original").unwrap();
        let root = temp.path().join("storage");
        fabric_project::register(&root, "directory-recovery", &source).unwrap();
        let root = root.canonicalize().unwrap();
        let recipe = temp.path().join("recipe.toml");
        fs::write(&recipe, "version=1\n[[steps]]\nargv=['/usr/bin/true']\n").unwrap();
        fabric_project::prepare::prepare(&root, "directory-recovery", &recipe).unwrap();
        let fake = temp.path().join("fake");
        fs::write(&fake, "#!/bin/sh\numask 077\nif [ \"$1\" = '--version' ]; then printf 'fabric-fake-harness 1\\n'; exit 0; fi\nrm gone\nprintf '\\000\\377\\012' > binary\nprintf recovered > .hidden\nmkdir -p nested/empty\nprintf 'worker log\\n'\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
        db.execute_batch(
            "CREATE TRIGGER test_lease AFTER UPDATE OF state ON workspaces
             WHEN NEW.state='CLEANUP_PENDING' BEGIN
             UPDATE workspaces SET lease_expires_at=9223372036854775807
             WHERE workspace_id=NEW.workspace_id; END;
             CREATE TRIGGER test_association BEFORE UPDATE OF artifact_manifest_id ON runs
             WHEN NEW.artifact_manifest_id IS NOT NULL
             BEGIN SELECT RAISE(ABORT, 'injected directory capture interruption'); END;",
        )
        .unwrap();
        drop(db);
        let prompt = temp.path().join("prompt");
        fs::write(&prompt, b"prompt").unwrap();
        let pin = crate::target::reference_pin();
        let result = crate::run::foreground(
            &root,
            "directory-recovery",
            &prompt,
            &fabric_runner::FakeHarness::new(fake).unwrap(),
            &crate::run::RunOptions {
                copy_limit_bytes: 1024 * 1024,
                harness_request: fabric_domain::HarnessRequest {
                    placement: pin.placement.clone(),
                    ..fabric_domain::HarnessRequest::default()
                },
                target_pin: Some(pin),
                ..crate::run::RunOptions::default()
            },
        );
        let failure = result.err().expect("artifact association must fail");
        assert!(
            failure
                .to_string()
                .contains("injected directory capture interruption")
        );
        let (_, store) = crate::workspace::open_store(&root).unwrap();
        let ids = store.workspace_ids().unwrap();
        assert_eq!(ids.len(), 1);
        let workspace = store.workspace(&ids[0]).unwrap().unwrap();
        let before = store.run(&workspace.run_id).unwrap().unwrap();
        assert!(fabric_project::is_directory_source(&before.base_commit_sha));
        assert!(before.capture.is_some());
        assert!(before.artifact_manifest_id.is_none());
        let tree = Path::new(&workspace.managed_path);
        assert!(!tree.join(".git").exists());
        assert_eq!(fs::read(tree.join("binary")).unwrap(), b"\0\xff\n");
        let destination = root.join("artifacts").join(format!("a-{}", before.run_id));
        let stage = root
            .join("artifact-staging")
            .join(format!("a-{}", before.run_id));
        // Recreate the durable boundary before capture started: output/completion
        // are recorded, but neither a partial stage nor promoted evidence exists.
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&destination).unwrap();
        assert!(!destination.exists());
        assert!(!stage.exists());
        let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
        db.execute_batch(
            "DROP TRIGGER test_lease; DROP TRIGGER test_association;
             UPDATE workspaces SET lease_expires_at=NULL;",
        )
        .unwrap();
        drop(db);
        let inspection = crate::workspace::recover(&root, &before.run_id, false).unwrap();
        assert_eq!(inspection.action, "would_recover", "{inspection:?}");
        assert!(!destination.exists());
        assert!(!stage.exists());
        assert_eq!(store.run(&before.run_id).unwrap().unwrap(), before);
        drop(store);

        let recovery = crate::workspace::recover(&root, &before.run_id, true).unwrap();
        assert_eq!(recovery.action, "reclaimed", "{recovery:?}");
        let (_, store) = crate::workspace::open_store(&root).unwrap();
        let recovered = store.run(&before.run_id).unwrap().unwrap();
        assert_eq!(
            verify_registered(&root, &store, &recovered).unwrap().len(),
            14
        );
        assert_eq!(
            store.workspace(&ids[0]).unwrap().unwrap().state,
            fabric_domain::WorkspaceState::Reclaimed
        );
        assert!(!tree.parent().unwrap().exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(destination.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["source_scope"], "directory_full_tree");
        for (name, _) in DIRECTORY_EMPTY_FILES {
            assert_eq!(fs::metadata(destination.join(name)).unwrap().len(), 0);
        }
        let restored = temp.path().join("restored");
        tar::Archive::new(File::open(destination.join("untracked.tar")).unwrap())
            .unpack(&restored)
            .unwrap();
        assert_eq!(fs::read(restored.join("binary")).unwrap(), b"\0\xff\n");
        assert_eq!(fs::read(restored.join(".hidden")).unwrap(), b"recovered");
        assert!(restored.join("nested/empty").is_dir());
        assert!(!restored.join("gone").exists());
        assert!(!source.join(".git").exists());
        assert_eq!(fs::read(source.join("gone")).unwrap(), b"original");
        drop(store);

        fn writable(path: &Path) {
            if fs::symlink_metadata(path).unwrap().is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                for entry in fs::read_dir(path).unwrap() {
                    writable(&entry.unwrap().path());
                }
            }
        }
        writable(temp.path());
    }
}
