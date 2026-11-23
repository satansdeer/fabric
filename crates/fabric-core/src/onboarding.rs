//! Revocable, local-only native-context references. No authentication contents are read.
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fabric_domain::{AgentProfile, ErrorKind, FabricError, HarnessId, HarnessPlan, HarnessRequest};
use fabric_runner::{HarnessAdapter, HarnessInfo, HarnessRegistry, NativeContext, RegistryOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_SELECTION: u64 = 64 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    version: u32,
    grant: String,
    profile: AgentProfile,
    fingerprint: String,
    identities: Vec<Identity>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    path: PathBuf,
    device: u64,
    inode: u64,
    directory: bool,
}

pub fn inspect(id: &HarnessId) -> Result<NativeContext, FabricError> {
    HarnessRegistry::default()
        .resolve(id, &RegistryOptions::default())?
        .native_context()
}

/// Called only after the caller obtains explicit consent for this exact description.
pub fn connect(root: &Path, context: &NativeContext) -> Result<(), FabricError> {
    let adapter = HarnessRegistry::default()
        .resolve(&context.profile.harness_id, &RegistryOptions::default())?;
    let mut inspected = context.clone();
    let controllers: Vec<_> = inspected
        .profile
        .native_config_refs
        .iter()
        .filter(|reference| reference.kind == fabric_runner::controller::REFERENCE)
        .cloned()
        .collect();
    inspected
        .profile
        .native_config_refs
        .retain(|reference| reference.kind != fabric_runner::controller::REFERENCE);
    let mut current = adapter.native_context()?;
    if json(&inspected)? != json(&current)? || !current.credential_available {
        return Err(authorization(
            "Native context is unavailable or changed; configure the harness locally, then inspect and connect again.",
        ));
    }
    current.profile.native_config_refs.extend(controllers);
    if let Some(controller) = fabric_runner::controller::load(&current.profile)? {
        if controller.root != root {
            return Err(authorization(
                "Controller storage root differs from the consent destination.",
            ));
        }
    }
    let request = HarnessRequest {
        profile: Some(current.profile.clone()),
        ..HarnessRequest::default()
    };
    let plan = adapter.plan(
        &request,
        &HarnessInfo {
            version: current.profile.harness_version.clone(),
        },
    )?;
    let identities = current
        .profile
        .native_config_refs
        .iter()
        .map(|reference| (&reference.path, true))
        .chain(current.credential_paths.iter().map(|path| (path, false)))
        .map(|(path, directory)| identity(path, directory))
        .collect::<Result<Vec<_>, _>>()?;
    let grant = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(failure)?
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let selection = Selection {
        version: 1,
        grant: grant.clone(),
        profile: current.profile,
        fingerprint: plan.profile.native_config_fingerprint,
        identities,
    };
    let bytes = json(&selection)?;
    if bytes.len() as u64 > MAX_SELECTION {
        return Err(authorization(
            "Native-context selection exceeds the local storage bound.",
        ));
    }
    existing_root(root)?;
    fabric_project::initialize(root)?;
    let directory = open_root(root)?;
    let name = selection_name(&selection.profile.harness_id)?;
    // Refuse insecure existing entries rather than overwriting a link or foreign file.
    read_selection(&directory, &selection.profile.harness_id)?;
    let temporary = CString::new(format!(".native-context-{grant}.tmp")).map_err(failure)?;
    // SAFETY: live directory descriptor and single-component NUL-terminated name.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(failure(std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned an exclusively owned new descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(&bytes).map_err(failure)?;
        file.sync_all().map_err(failure)?;
        read_selection(&directory, &selection.profile.harness_id)?;
        // SAFETY: both names are single components relative to the retained root.
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(failure(std::io::Error::last_os_error()));
        }
        directory.sync_all().map_err(failure)
    })();
    if result.is_err() {
        // SAFETY: remove only our uniquely created temporary entry, never its target.
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

pub fn revoke(root: &Path, id: &HarnessId) -> Result<(), FabricError> {
    let Some(directory) = existing_root(root)? else {
        return Ok(());
    };
    if read_selection(&directory, id)?.is_none() {
        return Ok(());
    }
    let name = selection_name(id)?;
    // SAFETY: only the checked single-component entry under the retained root is unlinked.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(failure(error));
        }
    }
    directory.sync_all().map_err(failure)
}

pub fn load(root: &Path, id: &HarnessId) -> Result<Option<AgentProfile>, FabricError> {
    Ok(load_selection(root, id)?.map(|(selection, _)| selection.profile))
}

fn load_selection(root: &Path, id: &HarnessId) -> Result<Option<(Selection, String)>, FabricError> {
    let Some(directory) = existing_root(root)? else {
        return Ok(None);
    };
    let Some(bytes) = read_selection(&directory, id)? else {
        return Ok(None);
    };
    let selection: Selection = serde_json::from_slice(&bytes).map_err(|_| {
        authorization("Invalid native-context selection; revoke it and connect again.")
    })?;
    selection.profile.validate()?;
    if selection.version != 1
        || selection.profile.harness_id != *id
        || selection.grant.is_empty()
        || selection.identities.is_empty()
        || selection.identities.len() > 64
        || selection.fingerprint.len() != 64
    {
        return Err(authorization(
            "Native-context selection identity is invalid; revoke it and connect again.",
        ));
    }
    for expected in &selection.identities {
        let current = identity(&expected.path, expected.directory)?;
        if expected.device != current.device || expected.inode != current.inode {
            return Err(authorization(
                "Selected native context was replaced; inspect and connect again.",
            ));
        }
    }
    Ok(Some((selection, format!("{:x}", Sha256::digest(&bytes)))))
}

/// The stamp belongs to the implicit selection, never to advanced explicit profiles.
pub fn plan(
    root: &Path,
    adapter: &dyn HarnessAdapter,
    mut request: HarnessRequest,
    info: &HarnessInfo,
) -> Result<(HarnessPlan, Option<String>), FabricError> {
    let implicit = request.profile.is_none();
    let selection = if implicit {
        load_selection(root, &adapter.id())?
    } else {
        None
    };
    if let Some((selection, _)) = &selection {
        request.profile = Some(selection.profile.clone());
    }
    let plan = adapter.plan(&request, info).map_err(|error| {
        if implicit && selection.is_none() && error.kind == ErrorKind::AuthorizationRequired {
            authorization(&format!("Run fabric harness connect {} to approve the native context, or supply an explicit advanced profile. {}", adapter.id(), error.message))
        } else {
            error
        }
    })?;
    if let Some((selection, _)) = &selection {
        if selection.fingerprint != plan.profile.native_config_fingerprint {
            return Err(authorization(
                "Selected native configuration changed; inspect and connect again.",
            ));
        }
    }
    Ok((plan, selection.map(|(_, stamp)| stamp)))
}

pub fn revalidate(root: &Path, plan: &HarnessPlan, stamp: Option<&str>) -> Result<(), FabricError> {
    let Some(stamp) = stamp else { return Ok(()) };
    let Some((_, current)) = load_selection(root, &plan.manifest.harness_id)? else {
        return Err(authorization(
            "Native-context consent was revoked; run fabric harness connect before launching.",
        ));
    };
    if stamp != current {
        return Err(authorization(
            "Native-context consent changed after planning; plan the request again.",
        ));
    }
    Ok(())
}

fn identity(path: &Path, directory: bool) -> Result<Identity, FabricError> {
    if !path.is_absolute() {
        return Err(authorization("Native-context references must be absolute."));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        authorization(
            "Selected native context is unavailable; configure it locally and connect again.",
        )
    })?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || path.canonicalize().map_err(failure)? != path
    {
        return Err(authorization(
            "Native-context references must resolve to the exact displayed real directory or regular credential file; repair links and connect again.",
        ));
    }
    Ok(Identity {
        path: path.into(),
        device: metadata.dev(),
        inode: metadata.ino(),
        directory,
    })
}

fn existing_root(root: &Path) -> Result<Option<File>, FabricError> {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failure(error)),
        Ok(_) => open_root(root).map(Some),
    }
}

fn open_root(root: &Path) -> Result<File, FabricError> {
    crate::workspace::private_directory(root)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(failure)?;
    private_metadata(&directory.metadata().map_err(failure)?, true)?;
    Ok(directory)
}

fn private_metadata(metadata: &std::fs::Metadata, directory: bool) -> Result<(), FabricError> {
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(authorization(
            "Native-context selection must be current-user-owned, owner-only and a real unlinked regular file.",
        ));
    }
    Ok(())
}

fn read_selection(directory: &File, id: &HarnessId) -> Result<Option<Vec<u8>>, FabricError> {
    let name = selection_name(id)?;
    // SAFETY: retained directory descriptor and a single-component name; no links followed.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(failure(error))
        };
    }
    // SAFETY: openat returned an exclusively owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let before = file.metadata().map_err(failure)?;
    private_metadata(&before, false)?;
    if before.len() > MAX_SELECTION {
        return Err(authorization(
            "Native-context selection exceeds the 64 KiB bound; revoke and reconnect.",
        ));
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(MAX_SELECTION + 1)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    let after = file.metadata().map_err(failure)?;
    if bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(authorization(
            "Native-context selection changed while reading; retry after reconnecting.",
        ));
    }
    Ok(Some(bytes))
}

fn selection_name(id: &HarnessId) -> Result<CString, FabricError> {
    CString::new(format!(".native-context-{id}.json")).map_err(failure)
}
fn json(value: &impl Serialize) -> Result<Vec<u8>, FabricError> {
    serde_json::to_vec(value).map_err(failure)
}
fn authorization(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::AuthorizationRequired,
        message: message.into(),
    }
}
fn failure(error: impl std::fmt::Display) -> FabricError {
    authorization(&format!(
        "Native-context selection unavailable: {error}; inspect local permissions or reconnect."
    ))
}

#[cfg(test)]
mod tests {
    mod reference_plugins {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fabric-runner/tests/support/reference_plugins.rs"
        ));
    }
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        fabric_runner::FakeHarness,
        HarnessInfo,
        AgentProfile,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let adapter =
            fabric_runner::FakeHarness::new(PathBuf::from("/unused-fixture-harness")).unwrap();
        let info = HarnessInfo {
            version: "fabric-fake-harness 1".into(),
        };
        let profile = AgentProfile {
            harness_id: adapter.id(),
            harness_version: info.version.clone(),
            native_config_refs: Vec::new(),
            model_policy: fabric_domain::ModelPolicy::NativeDefault {},
            skills: Vec::new(),
            plugins: Vec::new(),
            mcp: Vec::new(),
            connection_refs: Vec::new(),
        };
        (temporary, root, adapter, info, profile)
    }

    fn seed(
        root: &Path,
        adapter: &dyn HarnessAdapter,
        info: &HarnessInfo,
        profile: &AgentProfile,
    ) -> PathBuf {
        let request = HarnessRequest {
            profile: Some(profile.clone()),
            ..HarnessRequest::default()
        };
        let plan = adapter.plan(&request, info).unwrap();
        let selection = Selection {
            version: 1,
            grant: "fixture-grant".into(),
            profile: profile.clone(),
            fingerprint: plan.profile.native_config_fingerprint,
            identities: vec![identity(root, true).unwrap()],
        };
        let path = root.join(format!(".native-context-{}.json", adapter.id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(&json(&selection).unwrap()).unwrap();
        path
    }

    #[test]
    fn implicit_stamp_is_revoked_but_independent_explicit_profile_is_not() {
        let (_temporary, root, adapter, info, profile) = fixture();
        seed(&root, &adapter, &info, &profile);
        let (implicit, stamp) = plan(&root, &adapter, HarnessRequest::default(), &info).unwrap();
        assert!(stamp.is_some());
        revalidate(&root, &implicit, stamp.as_deref()).unwrap();
        let explicit = HarnessRequest {
            profile: Some(profile),
            ..HarnessRequest::default()
        };
        let (explicit, explicit_stamp) = plan(&root, &adapter, explicit, &info).unwrap();
        assert!(explicit_stamp.is_none());
        revoke(&root, &adapter.id()).unwrap();
        revoke(&root, &adapter.id()).unwrap();
        assert!(revalidate(&root, &implicit, stamp.as_deref()).is_err());
        revalidate(&root, &explicit, explicit_stamp.as_deref()).unwrap();
        assert!(load(&root, &adapter.id()).unwrap().is_none());
    }

    #[test]
    fn serialized_queued_plan_keeps_original_grant_after_reconnect() {
        let (_temporary, root, adapter, info, profile) = fixture();
        seed(&root, &adapter, &info, &profile);
        let accepted = plan(&root, &adapter, HarnessRequest::default(), &info).unwrap();
        let durable = serde_json::to_vec(&accepted).unwrap();
        revoke(&root, &adapter.id()).unwrap();
        let (queued, stamp): (HarnessPlan, Option<String>) =
            serde_json::from_slice(&durable).unwrap();
        assert!(revalidate(&root, &queued, stamp.as_deref()).is_err());
        let path = seed(&root, &adapter, &info, &profile);
        let mut new_selection: Selection =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        new_selection.grant = "replacement-grant".into();
        std::fs::write(path, json(&new_selection).unwrap()).unwrap();
        let fresh = plan(&root, &adapter, HarnessRequest::default(), &info).unwrap();
        revalidate(&root, &fresh.0, fresh.1.as_deref()).unwrap();
        assert!(revalidate(&root, &queued, stamp.as_deref()).is_err());
    }

    #[test]
    fn replacement_and_fingerprint_changes_require_new_consent() {
        let (_temporary, root, adapter, info, profile) = fixture();
        let path = seed(&root, &adapter, &info, &profile);
        let (planned, stamp) = plan(&root, &adapter, HarnessRequest::default(), &info).unwrap();
        let mut selection: Selection =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        selection.grant = "new-grant".into();
        std::fs::write(&path, json(&selection).unwrap()).unwrap();
        assert!(revalidate(&root, &planned, stamp.as_deref()).is_err());
        selection.fingerprint = "0".repeat(64);
        std::fs::write(&path, json(&selection).unwrap()).unwrap();
        assert!(plan(&root, &adapter, HarnessRequest::default(), &info).is_err());
        selection.fingerprint = planned.profile.native_config_fingerprint;
        selection.identities[0].inode ^= 1;
        std::fs::write(&path, json(&selection).unwrap()).unwrap();
        assert!(load(&root, &adapter.id()).is_err());
    }

    #[test]
    fn selection_reads_reject_permissions_links_nonfiles_and_oversize() {
        let (_temporary, root, adapter, info, profile) = fixture();
        let path = seed(&root, &adapter, &info, &profile);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&root, &adapter.id()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias = root.join("hardlink");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(load(&root, &adapter.id()).is_err());
        std::fs::remove_file(&alias).unwrap();
        std::fs::rename(&path, &alias).unwrap();
        std::os::unix::fs::symlink(&alias, &path).unwrap();
        assert!(load(&root, &adapter.id()).is_err());
        assert!(revoke(&root, &adapter.id()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(load(&root, &adapter.id()).is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&alias, &path).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SELECTION + 1)
            .unwrap();
        assert!(load(&root, &adapter.id()).is_err());
    }

    #[test]
    fn check_and_revoke_absent_root_never_create_it() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("absent");
        let id: HarnessId = "omp".parse().unwrap();
        assert!(load(&root, &id).unwrap().is_none());
        revoke(&root, &id).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn native_consent_drives_shared_run_and_blocks_command_time_revocation() {
        const CHILD: &str = "FABRIC_NATIVE_RUN_CHILD";
        if let Some(name) = std::env::var_os(CHILD) {
            let id: HarnessId = name.to_str().unwrap().parse().unwrap();
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("storage");
            let context = inspect(&id).unwrap();
            let mut stale = context.clone();
            stale.credential_available = false;
            assert!(connect(&root, &stale).is_err());
            assert!(!root.exists());
            connect(&root, &context).unwrap();
            let persisted = std::fs::read(root.join(format!(".native-context-{id}.json"))).unwrap();
            assert!(!String::from_utf8_lossy(&persisted).contains("opaque fixture authentication"));
            let repository = temporary.path().join("repo");
            std::fs::create_dir(&repository).unwrap();
            let git = |arguments: &[&str]| {
                let output = std::process::Command::new("/usr/bin/git")
                    .arg("-C")
                    .arg(&repository)
                    .args(arguments)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_AUTHOR_NAME", "Fixture")
                    .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                    .env("GIT_COMMITTER_NAME", "Fixture")
                    .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
            };
            git(&["init", "--quiet"]);
            std::fs::write(repository.join("tracked"), "fixture").unwrap();
            git(&["add", "."]);
            git(&["commit", "--quiet", "-m", "fixture"]);
            fabric_project::register(&root, "fixture", &repository).unwrap();
            let recipe = temporary.path().join("recipe.toml");
            std::fs::write(&recipe, "version=1\n[[steps]]\nargv=['/usr/bin/true']\n").unwrap();
            fabric_project::prepare::prepare(&root, "fixture", &recipe).unwrap();
            let prompt = temporary.path().join("prompt");
            std::fs::write(&prompt, "harmless fixture").unwrap();
            let adapter = HarnessRegistry::default()
                .resolve(&id, &RegistryOptions::default())
                .unwrap();
            let native = &context.profile.native_config_refs[0].path;
            let marker = native.join(".worker-launched");
            let options = crate::run::RunOptions {
                target_pin: Some(crate::target::reference_pin()),
                ..Default::default()
            };
            let report =
                crate::run::foreground(&root, "fixture", &prompt, adapter.as_ref(), &options)
                    .unwrap();
            assert_eq!(report.exit_code, Some(0));
            assert!(marker.exists());
            std::fs::remove_file(&marker).unwrap();
            let registry = HarnessRegistry::default();
            let registry_options = RegistryOptions::default();
            let batch_harnesses = crate::run::BatchHarnesses {
                registry: &registry,
                options: &registry_options,
                default: Some(&id),
            };
            let batch = crate::run::spawn_many(
                &root,
                "fixture",
                vec![crate::run::TaskInput {
                    id: "implicit-native".into(),
                    prompt_file: prompt.clone(),
                    harness: None,
                    request: None,
                }],
                1,
                &batch_harnesses,
                &options,
            )
            .unwrap();
            assert!(batch.succeeded);
            assert!(
                marker.exists(),
                "batch path did not apply its native selection"
            );
            std::fs::remove_file(&marker).unwrap();
            let config = native.join(if id.as_str() == "omp" {
                "settings.json"
            } else {
                "config.toml"
            });
            std::fs::write(&config, "").unwrap();
            assert!(
                crate::run::foreground(&root, "fixture", &prompt, adapter.as_ref(), &options)
                    .is_err()
            );
            assert!(!marker.exists());
            std::fs::remove_file(config).unwrap();
            connect(&root, &context).unwrap();

            struct Revoker<'a> {
                adapter: &'a dyn HarnessAdapter,
                root: &'a Path,
            }
            impl HarnessAdapter for Revoker<'_> {
                fn id(&self) -> HarnessId {
                    self.adapter.id()
                }
                fn probe(&self) -> Result<HarnessInfo, FabricError> {
                    self.adapter.probe()
                }
                fn plan(
                    &self,
                    request: &HarnessRequest,
                    info: &HarnessInfo,
                ) -> Result<HarnessPlan, FabricError> {
                    self.adapter.plan(request, info)
                }
                fn command(
                    &self,
                    spec: &fabric_runner::AgentSpec<'_>,
                ) -> Result<fabric_runner::CommandSpec, FabricError> {
                    let command = self.adapter.command(spec)?;
                    revoke(self.root, &self.id())?;
                    Ok(command)
                }
            }
            let revoker = Revoker {
                adapter: adapter.as_ref(),
                root: &root,
            };
            assert!(crate::run::foreground(&root, "fixture", &prompt, &revoker, &options).is_err());
            assert!(
                !marker.exists(),
                "revoked queued native selection launched a worker"
            );
            assert!(
                crate::run::foreground(&root, "fixture", &prompt, adapter.as_ref(), &options)
                    .is_err()
            );
            let explicit = crate::run::RunOptions {
                harness_request: HarnessRequest {
                    profile: Some(context.profile),
                    ..HarnessRequest::default()
                },
                ..options
            };
            let report =
                crate::run::foreground(&root, "fixture", &prompt, &revoker, &explicit).unwrap();
            assert_eq!(report.exit_code, Some(0));
            assert!(
                marker.exists(),
                "independent explicit profile was accidentally revoked"
            );
            return;
        }
        for (name, variable, auth, version) in [
            ("omp", "PI_CODING_AGENT_DIR", "agent.db", "omp/19.1.0"),
            ("codex", "CODEX_HOME", "auth.json", "codex-cli 0.171.0"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let home = temporary.path().join("home");
            let native = temporary.path().join("selected native");
            let bin = temporary.path().join("bin");
            for path in [&home, &native, &bin] {
                std::fs::create_dir(path).unwrap();
            }
            std::fs::write(
                home.join("unselected-secret"),
                "unrelated fixture credential",
            )
            .unwrap();
            std::fs::write(native.join(auth), "opaque fixture authentication").unwrap();
            let binary = bin.join(name);
            std::fs::write(&binary, format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '{version}'; exit 0; fi\n[ -z \"$OPENAI_API_KEY$CODEX_API_KEY$AWS_SECRET_ACCESS_KEY\" ] || exit 91\n[ ! -e \"$HOME/unselected-secret\" ] || exit 92\nselected=${{PI_CODING_AGENT_DIR:-$CODEX_HOME}}\n[ -n \"$selected\" ] || exit 93\n: > \"$selected/.worker-launched\"\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"fixture\"}}'\n"
            )).unwrap();
            std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "onboarding::tests::native_consent_drives_shared_run_and_blocks_command_time_revocation", "--nocapture"])
                .env_clear().env(CHILD, name).env("HOME", home).env(variable, native)
                .env("FABRIC_PLUGIN_REGISTRY", reference_plugins::install(temporary.path()))
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("OPENAI_API_KEY", "unapproved-fixture").env("CODEX_API_KEY", "unapproved-fixture")
                .env("AWS_SECRET_ACCESS_KEY", "unapproved-fixture").output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
