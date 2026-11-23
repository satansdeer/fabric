//! Full native harness wire contract. Credential values never cross this boundary.
use crate::plugin::{PluginManifest, PluginSession, failure};
use crate::{AgentSpec, CancellationToken, CommandSpec, HarnessInfo, NativeContext};
use fabric_domain::{
    AgentEvent, Capability, CapabilitySupport, ErrorKind, FabricError, HarnessCapabilityManifest,
    HarnessPlan, HarnessRequest, HarnessVersion,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePluginMetadata {
    pub executable: String,
    pub root_environment: Vec<String>,
    pub fingerprint: NativeFingerprint,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeFingerprint {
    pub domain: String,
    pub files: Vec<String>,
    /// First selected reference wins; suffix is relative to that authorized root.
    pub roots: Vec<(String, String)>,
}
impl NativePluginMetadata {
    pub(crate) fn validate(&self) -> Result<(), FabricError> {
        let name = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        if !name(&self.executable)
            || self.root_environment.len() > 8
            || self.root_environment.iter().any(|s| {
                !name(s)
                    || !(s.ends_with("_DIR") || s.ends_with("_HOME"))
                    || s.starts_with("XDG_")
                    || s.starts_with("LD_")
                    || s.starts_with("DYLD_")
            })
        {
            return Err(failure("invalid_native_metadata"));
        }
        let relative = |s: &str| {
            !s.contains('\0')
                && s.len() <= 256
                && !std::path::Path::new(s).is_absolute()
                && std::path::Path::new(s)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
        };
        if self.fingerprint.domain.is_empty()
            || self.fingerprint.domain.len() > 128
            || self.fingerprint.files.len() > 32
            || self
                .fingerprint
                .files
                .iter()
                .any(|f| !relative(f) || f.contains('/'))
            || self.fingerprint.roots.len() > 8
            || self
                .fingerprint
                .roots
                .iter()
                .any(|(kind, suffix)| !name(kind) || !relative(suffix))
        {
            return Err(failure("invalid_native_fingerprint"));
        }
        Ok(())
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeCallContext {
    pub binary: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub credentials: BTreeMap<String, bool>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReply<T> {
    pub result: Option<T>,
    pub failure: Option<ErrorKind>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlan {
    pub plan: HarnessPlan,
    pub supports_cost_limit: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum NativeArg {
    Literal(String),
    Prompt { prompt: bool },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCommand {
    pub program: PathBuf,
    pub args: Vec<NativeArg>,
    pub environment: Vec<(String, String)>,
}

pub(crate) fn context(
    manifest: &PluginManifest,
    request: Option<&HarnessRequest>,
    inspect: bool,
) -> Result<NativeCallContext, FabricError> {
    let metadata = manifest
        .native
        .as_ref()
        .ok_or_else(|| failure("missing_native_metadata"))?;
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let binary = crate::discover_executable(&metadata.executable, &path).ok_or_else(|| crate::execution_error(format!("Harness executable '{}' not found on PATH. Install the plugin's pinned CLI and add its directory to PATH; use fabric doctor to inspect discovery.", metadata.executable)))?;
    let mut environment = BTreeMap::new();
    environment.insert(
        "PATH".into(),
        crate::profile::runtime_environment(&binary)?
            .into_iter()
            .find(|(key, _)| key == "PATH")
            .ok_or_else(|| failure("missing_runtime_path"))?
            .1
            .into_string()
            .map_err(|_| failure("non_utf8_path"))?,
    );
    if inspect {
        for key in
            std::iter::once("HOME").chain(metadata.root_environment.iter().map(String::as_str))
        {
            if let Some(value) = std::env::var_os(key) {
                let value = value
                    .into_string()
                    .map_err(|_| failure("invalid_native_root"))?;
                if !std::path::Path::new(&value).is_absolute()
                    || value.chars().any(char::is_control)
                    || value.len() > 4096
                {
                    return Err(failure("invalid_native_root"));
                }
                environment.insert(key.to_owned(), value);
            }
        }
    }
    let credentials = request
        .and_then(|r| r.profile.as_ref())
        .map(|profile| {
            profile
                .connection_refs
                .iter()
                .filter(|r| r.kind == "env")
                .map(|r| {
                    (
                        r.name.clone(),
                        std::env::var_os(&r.name).is_some_and(|v| !v.is_empty()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(NativeCallContext {
        binary: Some(binary),
        environment,
        credentials,
    })
}
pub(crate) fn call<T: serde::de::DeserializeOwned>(
    manifest: &PluginManifest,
    method: &str,
    params: Value,
) -> Result<T, FabricError> {
    let reply: NativeReply<T> = crate::plugin::PluginHost::default().invoke(
        manifest,
        method,
        params,
        &CancellationToken::default(),
    )?;
    resolve_reply(reply)
}
pub(crate) fn resolve_reply<T>(reply: NativeReply<T>) -> Result<T, FabricError> {
    match (reply.result, reply.failure) {
        (Some(result), None) => Ok(result),
        (None, Some(kind)) if matches!(kind, ErrorKind::UnsupportedCapability | ErrorKind::UnsupportedVersion | ErrorKind::ModelUnsupported | ErrorKind::ModelUnverified | ErrorKind::UnsupportedPlacement | ErrorKind::InvalidProfile | ErrorKind::AuthorizationRequired | ErrorKind::InvalidArguments) => Err(FabricError { kind, message: match kind {
            ErrorKind::ModelUnverified => "MODEL_UNVERIFIED: external harness model override is unverified.".into(),
            ErrorKind::AuthorizationRequired => "External harness requires an explicitly authorized profile and available credential connections.".into(),
            ErrorKind::InvalidProfile => "External harness rejected the selected profile.".into(),
            _ => format!("External harness rejected request ({kind:?})."),
        } }),
        _ => Err(failure("invalid_native_reply")),
    }
}
pub(crate) fn capabilities(
    manifest: &PluginManifest,
    info: &HarnessInfo,
) -> Result<HarnessCapabilityManifest, FabricError> {
    let value: HarnessCapabilityManifest = call(
        manifest,
        "harness.capabilities",
        json!({"version":info.version}),
    )?;
    validate_capabilities(manifest, &value, info)?;
    Ok(value)
}
fn validate_capabilities(
    plugin: &PluginManifest,
    manifest: &HarnessCapabilityManifest,
    info: &HarnessInfo,
) -> Result<(), FabricError> {
    HarnessVersion::validate(&info.version)?;
    if manifest.harness_id != plugin.id
        || manifest.exact_version != info.version
        || manifest.supported_versions.is_empty()
        || manifest.supported_versions.len() > 4096
        || manifest.capabilities.len() != Capability::ALL.len()
    {
        return Err(failure("invalid_native_capabilities"));
    }
    for capability in Capability::ALL {
        let mut statuses = manifest
            .capabilities
            .iter()
            .filter(|s| s.capability == capability);
        let s = statuses
            .next()
            .ok_or_else(|| failure("invalid_native_capabilities"))?;
        if statuses.next().is_some() {
            return Err(failure("invalid_native_capabilities"));
        }
        if s.reason.code.is_empty()
            || s.reason.code.len() > 128
            || s.reason.message.is_empty()
            || s.reason.message.len() > 4096
            || s.minimum_version
                .as_ref()
                .is_some_and(|v| HarnessVersion::validate(v).is_err())
            || matches!(
                capability,
                Capability::ExecutionCancel
                    | Capability::WorkspaceExternal
                    | Capability::FilesystemPatchCapture
            ) && s.support != CapabilitySupport::FabricManaged
        {
            return Err(failure("invalid_native_capabilities"));
        }
    }
    Ok(())
}
pub(crate) fn plan(
    manifest: &PluginManifest,
    request: &HarnessRequest,
    info: &HarnessInfo,
) -> Result<HarnessPlan, FabricError> {
    request.validate()?;
    let reply: NativePlan = match call(
        manifest,
        "harness.plan",
        json!({"request":request,"version":info.version,"context":context(manifest, Some(request), false)?}),
    ) {
        Ok(reply) => reply,
        Err(error) if error.kind == ErrorKind::UnsupportedCapability => {
            let capabilities = capabilities(manifest, info)?;
            let effective = crate::profile::effective_request(request, &capabilities)?;
            crate::planning::validate_required_capabilities(&effective, &capabilities)?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    validate_capabilities(manifest, &reply.plan.manifest, info)?;
    let effective = crate::profile::effective_request(request, &reply.plan.manifest)?;
    let evidence = &reply.plan.profile;
    let source = &manifest.native.as_ref().unwrap().fingerprint;
    let root = source.roots.iter().find_map(|(kind, suffix)| {
        effective
            .profile
            .as_ref()?
            .native_config_refs
            .iter()
            .find(|r| &r.kind == kind)
            .map(|r| r.path.join(suffix))
    });
    if root.as_ref().is_some_and(|r| !clean_path(r)) {
        return Err(failure("invalid_native_profile"));
    }
    let files: Vec<_> = source.files.iter().map(String::as_str).collect();
    let fingerprint = crate::profile::native_fingerprint(
        root.as_deref(),
        source.domain.as_bytes(),
        &files,
        "Selected native config changed or is unsafe to fingerprint.",
    )?;
    if fingerprint != evidence.native_config_fingerprint {
        return Err(failure("invalid_native_fingerprint"));
    }
    if evidence.native_config_fingerprint.len() != 64
        || !evidence
            .native_config_fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || evidence.diagnostics.len() > 32
        || evidence
            .diagnostics
            .iter()
            .any(|d| d.code.len() > 128 || d.message.len() > 4096)
    {
        return Err(failure("invalid_native_profile"));
    }
    let expected = crate::profile::evidence(
        &effective,
        &reply.plan.manifest,
        evidence.native_config_fingerprint.clone(),
        evidence.portable,
        evidence.diagnostics.clone(),
    )?;
    if expected.digest != evidence.digest
        || serde_json::to_value(&effective).ok() != serde_json::to_value(&reply.plan.request).ok()
        || reply.plan.model_resolved.is_some()
    {
        return Err(failure("invalid_native_plan"));
    }
    crate::planning::negotiate_profile(
        &effective,
        reply.plan.manifest,
        reply.supports_cost_limit,
        expected,
    )
}
pub(crate) fn native_context(manifest: &PluginManifest) -> Result<NativeContext, FabricError> {
    let ctx = context(manifest, None, true)?;
    let granted: Vec<_> = ctx
        .environment
        .iter()
        .filter(|(k, _)| k.as_str() != "PATH")
        .map(|(_, v)| std::fs::canonicalize(v).unwrap_or_else(|_| PathBuf::from(v)))
        .collect();
    let value: NativeContext = call(manifest, "harness.native_context", json!({"context":ctx}))?;
    value.profile.validate()?;
    HarnessVersion::validate(&value.profile.harness_version)?;
    if value
        .profile
        .native_config_refs
        .iter()
        .any(|r| !clean_path(&r.path) || !granted.iter().any(|grant| r.path.starts_with(grant)))
        || value.credential_paths.iter().any(|p| !clean_path(p))
    {
        return Err(failure("ungranted_native_context"));
    }
    if value.profile.harness_id != manifest.id
        || value.profile.native_config_refs.is_empty()
        || value.credential_paths.len() > 32
        || value.exposure.is_empty()
        || value.exposure.len() > 4096
        || value
            .profile
            .connection_refs
            .iter()
            .any(|r| r.kind != "native-auth")
        || value.credential_paths.iter().any(|p| {
            !p.is_absolute()
                || !value
                    .profile
                    .native_config_refs
                    .iter()
                    .any(|r| p.starts_with(&r.path))
        })
    {
        return Err(failure("invalid_native_context"));
    }
    Ok(value)
}
pub(crate) fn command(
    manifest: &PluginManifest,
    spec: &AgentSpec<'_>,
) -> Result<CommandSpec, FabricError> {
    let version = spec
        .request
        .profile
        .as_ref()
        .ok_or_else(|| failure("native_profile_required"))?
        .harness_version
        .clone();
    crate::profile::revalidate(
        &crate::plugin::PluginHarness::new(manifest.clone())?,
        spec,
        &version,
    )?;
    let ctx = context(manifest, Some(spec.request), false)?;
    let binary = ctx.binary.clone().unwrap();
    let command: NativeCommand = call(
        manifest,
        "harness.command",
        json!({"request":spec.request,"profile":spec.profile,"prompt_file":spec.prompt_file,"context":ctx}),
    )?;
    if command.program != binary || command.args.len() > 128 || command.environment.len() > 64 {
        return Err(failure("invalid_native_command"));
    }
    let mut args = Vec::with_capacity(command.args.len());
    let mut prompt = None;
    for arg in command.args {
        args.push(match arg {
            NativeArg::Literal(s) if !s.contains('\0') && s.len() <= 4096 => OsString::from(s),
            NativeArg::Prompt { prompt: true } if prompt.is_none() => {
                let value = std::fs::read_to_string(spec.prompt_file)
                    .map_err(|_| failure("invalid_prompt"))?;
                if value.contains('\0') {
                    return Err(failure("invalid_prompt"));
                }
                prompt = Some(());
                value.into()
            }
            _ => return Err(failure("invalid_native_argv")),
        });
    }
    if prompt.is_none() {
        return Err(failure("missing_native_prompt"));
    }
    let profile = spec.request.profile.as_ref().unwrap();
    let metadata = manifest.native.as_ref().unwrap();
    let mut environment = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (key, value) in command.environment {
        if !seen.insert(key.clone())
            || key.is_empty()
            || key.len() > 128
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || value.contains('\0')
        {
            return Err(failure("invalid_native_environment"));
        }
        if key == "PATH" {
            continue;
        } // Host computes the bounded executable search path.
        if profile
            .connection_refs
            .iter()
            .any(|r| r.kind == "env" && r.name == key)
        {
            if key == "HOME"
                || key.starts_with("XDG_")
                || key.starts_with("LD_")
                || key.starts_with("DYLD_")
                || metadata.root_environment.contains(&key)
            {
                return Err(failure("unsafe_credential_environment"));
            }
            if value != "fabric-credential-available" {
                return Err(failure("invalid_credential_reference"));
            }
            let secret = std::env::var_os(&key)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| FabricError {
                    kind: ErrorKind::AuthorizationRequired,
                    message: "Selected credential is unavailable.".into(),
                })?;
            environment.push((key.into(), secret));
        } else if (key == "HOME" || metadata.root_environment.contains(&key))
            && clean_path(std::path::Path::new(&value))
            && profile
                .native_config_refs
                .iter()
                .any(|r| std::path::Path::new(&value).starts_with(&r.path))
        {
            environment.push((key.into(), value.into()));
        } else {
            return Err(failure("unsolicited_native_environment"));
        }
    }
    environment.extend(crate::profile::runtime_environment(&binary)?);
    Ok(CommandSpec {
        program: binary,
        args,
        environment,
    })
}
fn clean_path(path: &std::path::Path) -> bool {
    path.is_absolute()
        && !path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
}
pub(crate) fn normalize(
    session: &mut PluginSession,
    event: crate::NativeEvent<'_>,
) -> Result<Option<AgentEvent>, FabricError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Normalized {
        event: Option<AgentEvent>,
    }
    let params = match event {
        crate::NativeEvent::Json(value) => json!({"event":value}),
        crate::NativeEvent::Text(text) => json!({"text":text}),
    };
    let reply: NativeReply<Normalized> =
        serde_json::from_value(session.call("harness.normalize", params)?)
            .map_err(|_| failure("invalid_normalized_event"))?;
    let event = reply
        .result
        .filter(|_| reply.failure.is_none())
        .ok_or_else(|| failure("normalization_failed"))?
        .event;
    if let Some(event) = &event {
        let valid = match event {
            AgentEvent::Output { stream, text } => {
                matches!(stream.as_str(), "stdout" | "stderr")
                    && text.len() <= crate::jsonl::MAX_LINE_BYTES
            }
            AgentEvent::Tool { name, phase } => {
                !name.is_empty()
                    && name.len() <= 1024
                    && matches!(phase.as_str(), "start" | "update" | "end")
            }
            AgentEvent::Usage { cost_usd, .. } => {
                cost_usd.is_none_or(|v| v.is_finite() && v >= 0.0)
            }
            AgentEvent::Failure { code, message } => {
                !code.is_empty()
                    && code.len() <= 128
                    && message.len() <= crate::jsonl::MAX_LINE_BYTES
            }
            _ => false,
        };
        if !valid {
            return Err(failure("invalid_normalized_event"));
        }
    }
    Ok(event)
}
