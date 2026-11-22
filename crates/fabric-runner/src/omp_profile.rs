//! Pinned OMP-native local configuration and credential authorization.
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use fabric_domain::{
    CapabilityReason, ErrorKind, FabricError, HarnessCapabilityManifest, HarnessRequest,
    ProfileEvidence,
};
use sha2::{Digest, Sha256};

// Explicit finite subset of v19.1.0 documented provider variables, not arbitrary env names.
const ENV_KEYS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "OPENAI_CODEX_OAUTH_TOKEN",
    "GEMINI_API_KEY",
    "GROQ_API_KEY",
    "OPENROUTER_API_KEY",
    "MISTRAL_API_KEY",
    "XAI_API_KEY",
];
const CONFIG_FILES: &[&str] = &[
    "config.yml",
    "config.yaml",
    "settings.json",
    "models.yml",
    "models.yaml",
    "mcp.json",
    ".mcp.json",
];

fn authorization(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::AuthorizationRequired,
        message: message.into(),
    }
}

pub(super) fn resolve(
    request: &HarnessRequest,
    manifest: &HarnessCapabilityManifest,
    launch: bool,
) -> Result<(ProfileEvidence, Vec<(OsString, OsString)>), FabricError> {
    let profile = request.profile.as_ref().ok_or_else(|| authorization("OMP requires an explicit profile with an authorized native-auth or approved env connection; doctor --version does not require credentials."))?;
    let mut home: Option<&Path> = None;
    let mut agent: Option<PathBuf> = None;
    for reference in &profile.native_config_refs {
        if !reference.path.is_dir() {
            return Err(crate::profile::invalid(
                "OMP native config root must be an existing absolute directory.",
            ));
        }
        match reference.kind.as_str() {
            "native-home" if home.is_none() => home = Some(&reference.path),
            "native-agent-dir" if agent.is_none() => agent = Some(reference.path.clone()),
            _ => {
                return Err(crate::profile::invalid(
                    "OMP supports one native-home and one native-agent-dir reference only.",
                ));
            }
        }
    }
    let native = home.is_some() || agent.is_some();
    let agent = agent.or_else(|| home.map(|path| path.join(".omp/agent")));
    let mut environment = Vec::new();
    if let Some(home) = home {
        environment.push(("HOME".into(), home.as_os_str().to_owned()));
    }
    if let Some(agent) = &agent {
        environment.push(("PI_CODING_AGENT_DIR".into(), agent.as_os_str().to_owned()));
    }
    let native_auth_present = agent
        .as_ref()
        .and_then(|path| std::fs::metadata(path.join("agent.db")).ok())
        .is_some_and(|metadata| metadata.is_file() && metadata.len() > 0);
    if native_auth_present
        && !profile
            .connection_refs
            .iter()
            .any(|reference| reference.kind == "native-auth" && reference.name == "native-home")
    {
        return Err(authorization(
            "Selected native OMP config exposes an auth store; explicitly authorize native-auth/native-home or use an isolated env-only profile.",
        ));
    }
    let mut names = std::collections::HashSet::new();
    for reference in &profile.connection_refs {
        if !names.insert((&reference.kind, &reference.name)) {
            return Err(crate::profile::invalid(
                "Duplicate credential connection references are not allowed.",
            ));
        }
        match (reference.kind.as_str(), reference.name.as_str()) {
            ("native-auth", "native-home") if native => {
                if !native_auth_present {
                    return Err(authorization(
                        "Authorized OMP native auth store is unavailable; configure the selected native agent directory locally.",
                    ));
                }
            }
            ("env", name) if ENV_KEYS.contains(&name) => {
                let value = std::env::var_os(name).filter(|value| !value.is_empty()).ok_or_else(|| authorization("An approved OMP credential environment connection is missing or empty; configure the selected connection before planning."))?;
                if launch {
                    environment.push((name.into(), value));
                }
            }
            _ => {
                return Err(authorization(
                    "Unsupported OMP credential connection; select native-auth/native-home with a native root or a documented approved provider env name.",
                ));
            }
        }
    }
    if profile.connection_refs.is_empty() {
        return Err(authorization(
            "OMP requires at least one explicitly authorized available native-auth or approved env connection.",
        ));
    }
    let fingerprint = fingerprint(agent.as_deref())?;
    let mut diagnostics = vec![CapabilityReason {
        code: "auth_availability_only".into(),
        message: "Explicit credential connection availability checked locally; provider login validity, entitlement and model availability are not validated. No auth refresh, auth-store reads or command-key execution performed by Fabric.".into(),
    }];
    if native {
        diagnostics.push(CapabilityReason { code: "native_trusted_local".into(), message: "Explicit native config is trusted-local and may execute configured commands/extensions. It is not a sandbox or an atomic reproducible HOME snapshot; native auth/config mechanisms remain opaque.".into() });
        diagnostics.push(CapabilityReason { code: "native_fingerprint_coverage".into(), message: "Opaque bounded SHA-256 covers active-agent config.yml, config.yaml, settings.json, models.yml, models.yaml, mcp.json and .mcp.json (including absence), never agent.db or keychains. Plugin/skill/hook trees, other user/foreign roots, dotenv, DB migration settings and external services are not covered. Project config is covered separately by the pinned base commit. Native config may mutate after planning; command construction rechecks the fingerprint.".into() });
    } else {
        diagnostics.push(CapabilityReason { code: "private_runtime_home".into(), message: "Fresh private HOME/XDG, only explicitly selected approved env credentials. Project config/dotenv belongs to the trusted pinned workspace, not this user-config fingerprint; provider services are not reproducible snapshots.".into() });
    }
    Ok((
        crate::profile::evidence(request, manifest, fingerprint, !native, diagnostics)?,
        environment,
    ))
}

fn fingerprint(agent: Option<&Path>) -> Result<String, FabricError> {
    let mut hash = Sha256::new();
    hash.update(b"omp-native-config-v1");
    for name in CONFIG_FILES {
        hash.update(name.as_bytes());
        let Some(agent) = agent else {
            hash.update(b"absent");
            continue;
        };
        let path = agent.join(name);
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling selected config link is not an absent configuration.
                if std::fs::symlink_metadata(&path).is_ok() {
                    return Err(crate::profile::invalid(
                        "Selected OMP native config is unreadable.",
                    ));
                }
                hash.update(b"absent");
                continue;
            }
            Err(_) => {
                return Err(crate::profile::invalid(
                    "Selected OMP native config is unreadable.",
                ));
            }
        };
        let failure = || {
            crate::profile::invalid(
                "Selected OMP native config changed, is unreadable or exceeds the 1 MiB per-file bound.",
            )
        };
        let before = file.metadata().map_err(|_| failure())?;
        if !before.is_file() || before.len() > 1024 * 1024 {
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
