use std::ffi::OsString;
use std::path::Path;

use fabric_domain::{
    AgentProfile, CapabilityReason, ErrorKind, FabricError, HarnessCapabilityManifest,
    HarnessRequest, ModelPolicy, ProfileEvidence,
};
use sha2::{Digest, Sha256};

pub fn invalid(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidProfile,
        message: message.into(),
    }
}

pub fn native_root(variable: &str, default: &str) -> Result<std::path::PathBuf, FabricError> {
    let path = match std::env::var_os(variable) {
        Some(value) if !value.is_empty() => std::path::PathBuf::from(value),
        Some(_) => {
            return Err(invalid(
                "Native config directory override is empty; configure an absolute native directory.",
            ));
        }
        None => std::path::PathBuf::from(
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    invalid(
                        "HOME is unavailable; explicitly configure the harness native directory.",
                    )
                })?,
        )
        .join(default),
    };
    if !path.is_absolute() {
        return Err(invalid(
            "Native config directory must be absolute; set the harness directory override explicitly.",
        ));
    }
    // Resolve selected directory links once, before displaying the exact consent target.
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(_) => Err(invalid(
            "Native config directory metadata is inaccessible; repair its local permissions.",
        )),
    }
}

pub fn effective_request(
    request: &HarnessRequest,
    manifest: &HarnessCapabilityManifest,
) -> Result<HarnessRequest, FabricError> {
    request.validate()?;
    let mut effective = request.clone();
    if let Some(profile) = &request.profile {
        if profile.harness_id != manifest.harness_id
            || profile.harness_version != manifest.exact_version
        {
            return Err(invalid(
                "Profile must bind the selected harness and exact supported version.",
            ));
        }
        if let ModelPolicy::Exact { model } = &profile.model_policy {
            if request
                .model
                .as_ref()
                .is_some_and(|requested| requested != model)
            {
                return Err(invalid(
                    "Profile exact model conflicts with the request model.",
                ));
            }
            effective.model = Some(model.clone());
        }
    }
    Ok(effective)
}

pub fn evidence(
    request: &HarnessRequest,
    manifest: &HarnessCapabilityManifest,
    fingerprint: String,
    portable: bool,
    diagnostics: Vec<CapabilityReason>,
) -> Result<ProfileEvidence, FabricError> {
    let default_profile = request.profile.is_none().then(|| AgentProfile {
        harness_id: manifest.harness_id.clone(),
        harness_version: manifest.exact_version.clone(),
        native_config_refs: Vec::new(),
        model_policy: request
            .model
            .as_ref()
            .map_or(ModelPolicy::NativeDefault {}, |model| ModelPolicy::Exact {
                model: model.clone(),
            }),
        skills: Vec::new(),
        plugins: Vec::new(),
        mcp: Vec::new(),
        connection_refs: Vec::new(),
    });
    let profile = request
        .profile
        .as_ref()
        .or(default_profile.as_ref())
        .expect("explicit or managed profile");
    let canonical = serde_json::to_vec(&(
        profile,
        &manifest.harness_id,
        &manifest.exact_version,
        &request.model,
        &fingerprint,
    ))
    .map_err(|_| invalid("Cannot compute profile evidence."))?;
    Ok(ProfileEvidence {
        digest: format!("{:x}", Sha256::digest(canonical)),
        native_config_fingerprint: fingerprint,
        portable,
        diagnostics,
    })
}

pub fn managed_evidence(
    request: &HarnessRequest,
    manifest: &HarnessCapabilityManifest,
) -> Result<ProfileEvidence, FabricError> {
    if request.profile.as_ref().is_some_and(|profile| {
        !profile.native_config_refs.is_empty() || !profile.connection_refs.is_empty()
    }) {
        return Err(invalid(
            "This adapter cannot honor native config or credential connection references.",
        ));
    }
    evidence(request, manifest, format!("{:x}", Sha256::digest(b"fabric-private-home-v1")), true, vec![CapabilityReason {
        code: "private_runtime_home".into(), message: "Core-scoped private HOME/XDG (fresh unless explicitly bound to a durable environment); no native user config or credentials inherited. Project config belongs to the pinned base commit, not this fingerprint.".into(),
    }])
}

pub fn revalidate(
    adapter: &dyn crate::HarnessAdapter,
    spec: &crate::AgentSpec<'_>,
    version: &str,
) -> Result<fabric_domain::HarnessPlan, FabricError> {
    let plan = adapter.plan(
        spec.request,
        &crate::HarnessInfo {
            version: version.into(),
        },
    )?;
    if spec.profile.digest.is_empty()
        || plan.profile.digest != spec.profile.digest
        || spec.max_cost_usd != plan.request.max_cost_usd
    {
        return Err(invalid(
            "Command profile binding changed; plan the request again before launch.",
        ));
    }
    Ok(plan)
}

pub fn private_environment(
    command: &mut std::process::Command,
) -> Result<tempfile::TempDir, FabricError> {
    let root = tempfile::tempdir().map_err(|_| invalid("Cannot create private runtime HOME."))?;
    command.env("HOME", root.path());
    for (name, directory) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let path = root.path().join(directory);
        std::fs::create_dir(&path)
            .map_err(|_| invalid("Cannot create private runtime XDG directory."))?;
        command.env(name, path);
    }
    Ok(root)
}

pub fn runtime_environment(binary: &Path) -> Result<Vec<(OsString, OsString)>, FabricError> {
    let mut paths = vec![std::path::PathBuf::from("/usr/bin"), "/bin".into()];
    if let Some(parent) = binary.parent() {
        paths.push(parent.to_owned());
    }
    if let Some(bun) =
        std::env::var_os("PATH").and_then(|path| crate::discover_executable("bun", &path))
    {
        if let Some(parent) = bun.parent() {
            paths.push(parent.to_owned());
        }
    }
    let path = std::env::join_paths(paths).map_err(|_| {
        invalid("Executable runtime directory cannot be represented safely on PATH.")
    })?;
    Ok(vec![("PATH".into(), path)])
}

/// Hash only the adapter's finite non-auth file list, with bounded streaming reads.
pub fn native_fingerprint(
    root: Option<&Path>,
    domain: &[u8],
    files: &[&str],
    message: &str,
) -> Result<String, FabricError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut hash = Sha256::new();
    hash.update(domain);
    for name in files {
        hash.update(name.as_bytes());
        let Some(root) = root else {
            hash.update(b"absent");
            continue;
        };
        let failure = || invalid(message);
        let path = root.join(name);
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&path).is_ok() {
                    return Err(failure());
                }
                hash.update(b"absent");
                continue;
            }
            Err(_) => return Err(failure()),
        };
        let before = file.metadata().map_err(|_| failure())?;
        if !before.is_file() || before.nlink() != 1 || before.len() > 1024 * 1024 {
            return Err(failure());
        }
        hash.update(b"present");
        hash.update(before.len().to_le_bytes());
        let mut total = 0_u64;
        let mut buffer = [0_u8; 8192];
        loop {
            let count = file.read(&mut buffer).map_err(|_| failure())?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > 1024 * 1024 {
                return Err(failure());
            }
            hash.update(&buffer[..count]);
        }
        let same = |after: &std::fs::Metadata| {
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec()
        };
        if total != before.len()
            || !same(&file.metadata().map_err(|_| failure())?)
            || !same(&std::fs::metadata(path).map_err(|_| failure())?)
        {
            return Err(failure());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
