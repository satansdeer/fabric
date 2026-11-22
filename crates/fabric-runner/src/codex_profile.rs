//! Explicit Codex configuration and credential scope; auth stores are metadata-only.
use std::ffi::OsString;
use std::path::Path;

use fabric_domain::{
    CapabilityReason, ErrorKind, FabricError, HarnessCapabilityManifest, HarnessRequest,
    ProfileEvidence,
};

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
    let profile = request.profile.as_ref().ok_or_else(|| authorization(
        "Codex requires an explicit profile authorizing native-auth/native-config-dir or env/CODEX_API_KEY; doctor --version is credential-free.",
    ))?;
    let mut config: Option<&Path> = None;
    for reference in &profile.native_config_refs {
        if reference.kind != "native-config-dir"
            || config.is_some()
            || !reference.path.is_absolute()
            || !reference.path.is_dir()
        {
            return Err(crate::profile::invalid(
                "Codex supports one native-config-dir reference to an existing absolute directory only.",
            ));
        }
        config = Some(&reference.path);
    }
    // Even an env-authorized run must not silently gain access to selected native auth.
    let auth_metadata = if let Some(config) = config {
        let path = config.join("auth.json");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Some(std::fs::metadata(path).map_err(|_| authorization(
                "Selected Codex native auth store metadata is unavailable; select an isolated env-only profile or repair the authorized store locally.",
            ))?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(authorization("Selected Codex native auth store metadata is unavailable.")),
        }
    } else {
        None
    };
    let native_authorized = profile
        .connection_refs
        .iter()
        .any(|reference| reference.kind == "native-auth" && reference.name == "native-config-dir");
    if auth_metadata.is_some() && !native_authorized {
        return Err(authorization(
            "Selected Codex config exposes an auth store; explicitly authorize native-auth/native-config-dir or use an isolated env-only profile.",
        ));
    }
    let mut environment = Vec::new();
    if let Some(config) = config {
        environment.push(("CODEX_HOME".into(), config.as_os_str().to_owned()));
    }
    let mut names = std::collections::HashSet::new();
    for reference in &profile.connection_refs {
        if !names.insert((&reference.kind, &reference.name)) {
            return Err(crate::profile::invalid(
                "Duplicate credential connection references are not allowed.",
            ));
        }
        match (reference.kind.as_str(), reference.name.as_str()) {
            ("native-auth", "native-config-dir") if config.is_some() => {
                if !auth_metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.is_file())
                {
                    return Err(authorization(
                        "Authorized Codex native auth store is unavailable; a regular auth.json in the selected native-config-dir is required. Keychain state is not inspected.",
                    ));
                }
            }
            ("env", "CODEX_API_KEY") => {
                let value = std::env::var_os("CODEX_API_KEY")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| authorization("Approved Codex env/CODEX_API_KEY connection is missing or empty; configure the selected connection before planning."))?;
                if launch {
                    environment.push(("CODEX_API_KEY".into(), value));
                }
            }
            _ => {
                return Err(authorization(
                    "Unsupported Codex credential connection; select env/CODEX_API_KEY or native-auth/native-config-dir with an explicit native config root.",
                ));
            }
        }
    }
    if profile.connection_refs.is_empty() {
        return Err(authorization(
            "Codex requires at least one explicitly authorized available credential connection.",
        ));
    }
    let fingerprint = crate::profile::native_fingerprint(
        config,
        b"codex-native-config-v1",
        &["config.toml"],
        "Selected Codex config.toml changed, is unreadable or exceeds the 1 MiB bound.",
    )?;
    let diagnostics = vec![
        CapabilityReason {
            code: "auth_availability_only".into(),
            message: "Only explicit credential availability is checked locally, never provider authentication, entitlement or model access. Fabric does not open, hash or copy auth.json or inspect keychains.".into(),
        },
        CapabilityReason {
            code: if config.is_some() { "native_trusted_local" } else { "private_runtime_home" }.into(),
            message: if config.is_some() {
                "Private HOME/XDG with explicitly selected CODEX_HOME and native config preserved. Trusted-local: configured providers, commands and native auth mechanisms remain opaque, not sandboxed or remotely portable."
            } else {
                "Fresh private HOME/XDG and default native config root under that HOME, --ignore-user-config, only explicit env/CODEX_API_KEY; no ambient CODEX_HOME, foreign credentials or proxies."
            }.into(),
        },
        CapabilityReason {
            code: "native_fingerprint_coverage".into(),
            message: "Bounded streaming SHA-256 covers only selected config.toml including absence; never auth.json or keychains. Managed/system/project config, rules, named profiles, skills/plugins, MCP services and provider state are excluded. Project files belong to the pinned base commit. Mutable trusted-local config is rechecked at command construction, not an atomic snapshot.".into(),
        },
    ];
    Ok((
        crate::profile::evidence(request, manifest, fingerprint, false, diagnostics)?,
        environment,
    ))
}
