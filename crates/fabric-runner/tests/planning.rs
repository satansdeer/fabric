use fabric_domain::{
    Capability, CapabilitySupport, ErrorKind, FabricError, HarnessId, HarnessRequest,
};
use fabric_runner::{
    AgentSpec, CommandSpec, FakeHarness, HarnessAdapter, HarnessInfo, HarnessRegistry, negotiate,
};

fn fake() -> FakeHarness {
    FakeHarness::new("/must/not/launch".into()).unwrap()
}

fn info() -> HarnessInfo {
    HarnessInfo {
        version: "fabric-fake-harness 1".into(),
    }
}

#[test]
fn fake_plans_without_spawning_and_reports_all_capabilities_honestly() {
    let adapter = fake();
    let plan = adapter.plan(&HarnessRequest::default(), &info()).unwrap();
    assert!(plan.model_resolved.is_none());
    assert_eq!(plan.manifest.exact_version, "fabric-fake-harness 1");
    assert_eq!(plan.manifest.supported_versions, "fabric-fake-harness 1");
    assert_eq!(plan.manifest.capabilities.len(), 13);
    for capability in Capability::ALL {
        let statuses: Vec<_> = plan
            .manifest
            .capabilities
            .iter()
            .filter(|status| status.capability == capability)
            .collect();
        assert_eq!(statuses.len(), 1);
        let status = statuses[0];
        assert!(!status.reason.code.is_empty());
        assert!(!status.reason.message.is_empty());
        let expected = match capability {
            Capability::HeadlessRun
            | Capability::ExecutionCancel
            | Capability::WorkspaceExternal
            | Capability::FilesystemPatchCapture => CapabilitySupport::FabricManaged,
            Capability::EventsStructured => CapabilitySupport::Native,
            _ => CapabilitySupport::Unsupported,
        };
        assert_eq!(status.support, expected);
        let request = HarnessRequest {
            required_capabilities: vec![capability],
            ..HarnessRequest::default()
        };
        let result = adapter.plan(&request, &info());
        if expected == CapabilitySupport::Unsupported {
            assert_eq!(result.unwrap_err().kind, ErrorKind::UnsupportedCapability);
            assert!(status.minimum_version.is_none());
        } else {
            assert!(result.is_ok());
            assert_eq!(
                status.minimum_version.as_deref(),
                Some("fabric-fake-harness 1")
            );
        }
    }
}

#[test]
fn fake_rejects_unavailable_requests_before_command_construction() {
    for request in [
        HarnessRequest {
            model: Some("never-default-this".into()),
            ..HarnessRequest::default()
        },
        HarnessRequest {
            resume: Some("never-recap-this".into()),
            ..HarnessRequest::default()
        },
        HarnessRequest {
            hard_read_only: true,
            ..HarnessRequest::default()
        },
        HarnessRequest {
            access: "inspect".into(),
            ..HarnessRequest::default()
        },
        HarnessRequest {
            access: "publish".into(),
            ..HarnessRequest::default()
        },
        HarnessRequest {
            max_cost_usd: Some(0.01),
            ..HarnessRequest::default()
        },
    ] {
        assert_eq!(
            fake().plan(&request, &info()).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
    }
    for placement in ["remote", "byoc"] {
        let request = HarnessRequest {
            placement: placement.into(),
            ..HarnessRequest::default()
        };
        assert_eq!(
            fake().plan(&request, &info()).unwrap_err().kind,
            ErrorKind::UnsupportedPlacement
        );
    }
    assert_eq!(
        fake()
            .capabilities(&HarnessInfo {
                version: "fabric-fake-harness 2".into(),
            })
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedVersion
    );
}

#[test]
fn configuration_and_missing_lifecycle_attestations_never_downgrade() {
    let manifest = fake().capabilities(&info()).unwrap();
    for capability in [
        Capability::HeadlessRun,
        Capability::WorkspaceExternal,
        Capability::ExecutionCancel,
        Capability::FilesystemPatchCapture,
    ] {
        let mut incomplete = manifest.clone();
        incomplete
            .capabilities
            .retain(|status| status.capability != capability);
        assert_eq!(
            negotiate(&HarnessRequest::default(), incomplete, false)
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedCapability
        );
    }
    let mut configured = manifest;
    let status = configured
        .capabilities
        .iter_mut()
        .find(|status| status.capability == Capability::ModelOverride)
        .unwrap();
    status.support = CapabilitySupport::RequiresConfiguration;
    status.reason.code = "connection_required".into();
    status.reason.message = "Configure a provider connection before requesting a model.".into();
    let failure = negotiate(
        &HarnessRequest {
            model: Some("requested".into()),
            ..HarnessRequest::default()
        },
        configured,
        false,
    )
    .unwrap_err();
    assert_eq!(failure.kind, ErrorKind::UnsupportedCapability);
    assert!(failure.message.contains("Configure a provider connection"));
}

#[test]
fn unattested_adapters_default_to_unsupported_and_registry_ids_are_sorted() {
    struct Unattested;
    impl HarnessAdapter for Unattested {
        fn id(&self) -> HarnessId {
            "unattested".parse().unwrap()
        }
        fn probe(&self) -> Result<HarnessInfo, FabricError> {
            Ok(HarnessInfo {
                version: "unattested 1".into(),
            })
        }
        fn command(&self, _: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
            panic!("Planning must not construct a command")
        }
    }
    let info = Unattested.probe().unwrap();
    let manifest = Unattested.capabilities(&info).unwrap();
    assert_eq!(manifest.capabilities.len(), 13);
    assert!(manifest.capabilities.iter().all(|status| {
        status.support == CapabilitySupport::Unsupported && status.reason.code == "not_attested"
    }));
    assert_eq!(
        Unattested
            .plan(&HarnessRequest::default(), &info)
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedCapability
    );
    let ids = HarnessRegistry::default().ids();
    assert_eq!(
        ids.iter().map(HarnessId::as_str).collect::<Vec<_>>(),
        ["fake", "omp"]
    );
}
