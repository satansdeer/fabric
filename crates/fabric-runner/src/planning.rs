use fabric_domain::{
    Capability, CapabilityReason, CapabilitySupport, ErrorKind, FabricError,
    HarnessCapabilityManifest, HarnessId, HarnessPlan, HarnessRequest, HarnessVersion,
};

use crate::HarnessInfo;

/// Negotiate the trusted-local guarantees before any workspace or worker allocation.
/// Cost limiting is separate from the capability vocabulary and must be attested by the adapter.
pub fn negotiate(
    request: &HarnessRequest,
    manifest: HarnessCapabilityManifest,
    supports_cost_limit: bool,
) -> Result<HarnessPlan, FabricError> {
    request.validate()?;
    if request.placement != "local" {
        return Err(FabricError {
            kind: ErrorKind::UnsupportedPlacement,
            message: format!("Placement {} has no execution backend.", request.placement),
        });
    }
    if request.hard_read_only || request.access != "edit" {
        return Err(FabricError {
            kind: ErrorKind::UnsupportedCapability,
            message: "The trusted-local backend supports edit access only; it cannot enforce read-only access or authorize publication.".into(),
        });
    }
    if request.max_cost_usd.is_some() && !supports_cost_limit {
        return Err(FabricError {
            kind: ErrorKind::UnsupportedCapability,
            message: format!(
                "Harness {} has no attested cost limit.",
                manifest.harness_id
            ),
        });
    }
    let required = [
        Capability::HeadlessRun,
        Capability::WorkspaceExternal,
        Capability::ExecutionCancel,
        Capability::FilesystemPatchCapture,
    ];
    for capability in required
        .iter()
        .chain(&request.required_capabilities)
        .copied()
        .chain(request.model.as_ref().map(|_| Capability::ModelOverride))
        .chain(request.resume.as_ref().map(|_| Capability::SessionResume))
    {
        let status = manifest
            .capabilities
            .iter()
            .find(|status| status.capability == capability);
        if !status.is_some_and(|status| {
            matches!(
                status.support,
                CapabilitySupport::Native | CapabilitySupport::FabricManaged
            )
        }) {
            let reason = status.map_or("No capability attestation is present.", |status| {
                status.reason.message.as_str()
            });
            return Err(FabricError {
                kind: ErrorKind::UnsupportedCapability,
                message: format!(
                    "Harness {} cannot provide {}: {reason}",
                    manifest.harness_id,
                    capability.as_str()
                ),
            });
        }
    }
    Ok(HarnessPlan {
        request: request.clone(),
        manifest,
        model_resolved: None,
    })
}

pub(crate) fn check_version(
    harness_id: &str,
    version: &str,
    supported_version: &str,
) -> Result<(), FabricError> {
    HarnessVersion::validate(version)?;
    if version != supported_version {
        return Err(FabricError {
            kind: ErrorKind::UnsupportedVersion,
            message: format!(
                "Harness {harness_id} version {version:?} is unsupported; tested exact version: {supported_version}."
            ),
        });
    }
    Ok(())
}

pub(crate) fn local_manifest(
    harness_id: HarnessId,
    info: &HarnessInfo,
    supported_version: &str,
    headless_support: CapabilitySupport,
) -> Result<HarnessCapabilityManifest, FabricError> {
    check_version(harness_id.as_str(), &info.version, supported_version)?;
    let capabilities = Capability::ALL
        .into_iter()
        .map(|capability| {
            let (support, code, message) = match capability {
                Capability::HeadlessRun => (
                    headless_support.clone(),
                    "tested_headless_protocol",
                    "The version-pinned adapter launches the tested noninteractive protocol.",
                ),
                Capability::EventsStructured => (
                    CapabilitySupport::Native,
                    "tested_jsonl_protocol",
                    "The version-pinned adapter captures native JSONL events.",
                ),
                Capability::ExecutionCancel => (
                    CapabilitySupport::FabricManaged,
                    "process_group_supervision",
                    "Fabric terminates and reaps the local process group.",
                ),
                Capability::WorkspaceExternal => (
                    CapabilitySupport::FabricManaged,
                    "owned_workspace_cwd",
                    "Fabric launches the process in its validated owned workspace.",
                ),
                Capability::FilesystemPatchCapture => (
                    CapabilitySupport::FabricManaged,
                    "workspace_patch_capture",
                    "Fabric captures the owned Git workspace changes as artifacts.",
                ),
                Capability::UsageReport => (
                    CapabilitySupport::Unsupported,
                    "usage_unknown",
                    "Captured output does not establish complete normalized usage; usage remains unknown.",
                ),
                _ => (
                    CapabilitySupport::Unsupported,
                    "not_verified",
                    "This adapter has no implemented and verified path for this capability.",
                ),
            };
            let minimum_version = matches!(
                support,
                CapabilitySupport::Native | CapabilitySupport::FabricManaged
            )
            .then(|| supported_version.into());
            fabric_domain::CapabilityStatus {
                capability,
                support,
                reason: CapabilityReason {
                    code: code.into(),
                    message: message.into(),
                },
                minimum_version,
            }
        })
        .collect();
    Ok(HarnessCapabilityManifest {
        harness_id,
        exact_version: info.version.clone(),
        supported_versions: supported_version.into(),
        capabilities,
    })
}
