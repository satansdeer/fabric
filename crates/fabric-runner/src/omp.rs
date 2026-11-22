//! Installed OMP print-mode adapter. No shell, config copying or credential discovery.
use std::ffi::OsString;
use std::path::PathBuf;

use fabric_domain::{
    CapabilitySupport, FabricError, HarnessCapabilityManifest, HarnessId, HarnessPlan,
    HarnessRequest,
};

use crate::{AgentSpec, CommandSpec, HarnessAdapter, HarnessInfo, execution_error};

const SUPPORTED_VERSION: &str = "omp/19.1.0";

pub struct OmpHarness {
    binary: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl OmpHarness {
    pub fn discover() -> Result<Self, FabricError> {
        let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        let binary = crate::discover_executable("omp", &path).ok_or_else(|| {
            execution_error("OMP executable not found on PATH. Install OMP and add its executable directory to PATH; use fabric doctor to inspect discovery.")
        })?;
        // HOME/XDG enable OMP's normal local config/auth mechanism without copying
        // credentials. No API keys, runtime injection or arbitrary OMP overrides.
        let environment = ["PATH", "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "TMPDIR"]
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name.into(), value)))
            .collect();
        Ok(Self {
            binary,
            environment,
        })
    }
}

impl HarnessAdapter for OmpHarness {
    fn id(&self) -> HarnessId {
        "omp".parse().expect("registered harness ID")
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = crate::probe_version(&self.binary, self.environment.clone())?;
        crate::planning::check_version("omp", &version, SUPPORTED_VERSION)?;
        Ok(HarnessInfo { version })
    }

    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        crate::planning::local_manifest(
            self.id(),
            info,
            SUPPORTED_VERSION,
            CapabilitySupport::Native,
        )
    }

    fn plan(
        &self,
        request: &HarnessRequest,
        info: &HarnessInfo,
    ) -> Result<HarnessPlan, FabricError> {
        crate::negotiate(request, self.capabilities(info)?, true)
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        let prompt = std::fs::read_to_string(spec.prompt_file)
            .map_err(|_| execution_error("OMP prompt must be a readable UTF-8 file."))?;
        if prompt.contains('\0') {
            return Err(execution_error(
                "OMP prompt cannot contain NUL in an argv value.",
            ));
        }
        let mut args = vec![
            "-p".into(),
            "--mode".into(),
            "json".into(),
            "--no-session".into(),
            "--no-title".into(),
        ];
        if let Some(cost) = spec.max_cost_usd {
            args.push(format!("--max-cost={cost}").into());
        }
        args.extend(["--".into(), prompt.into()]);
        Ok(CommandSpec {
            program: self.binary.clone(),
            args,
            environment: self.environment.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn probe_rejects_unsupported_omp_version_with_typed_failure() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("omp");
        fs::write(&binary, "#!/bin/sh\nprintf 'omp/99.0.0\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let harness = OmpHarness {
            binary,
            environment: Vec::new(),
        };
        assert!(matches!(
            harness.probe(),
            Err(FabricError {
                kind: fabric_domain::ErrorKind::UnsupportedVersion,
                ..
            })
        ));
    }

    #[test]
    fn exact_version_plan_preserves_native_cost_argv_and_rejects_unverified_features() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("omp");
        fs::write(&binary, "#!/bin/sh\nprintf 'omp/19.1.0\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let harness = OmpHarness {
            binary,
            environment: vec![("HOME".into(), "/trusted/local/home".into())],
        };
        let info = harness.probe().unwrap();
        assert_eq!(info.version, "omp/19.1.0");
        let request = HarnessRequest {
            max_cost_usd: Some(0.25),
            ..HarnessRequest::default()
        };
        let plan = harness.plan(&request, &info).unwrap();
        assert_eq!(plan.manifest.capabilities.len(), 13);
        assert_eq!(plan.manifest.supported_versions, "omp/19.1.0");
        assert!(plan.model_resolved.is_none());
        let prompt = temp.path().join("prompt");
        fs::write(&prompt, "literal --prompt").unwrap();
        let command = harness
            .command(&AgentSpec {
                prompt_file: &prompt,
                max_cost_usd: plan.request.max_cost_usd,
            })
            .unwrap();
        assert_eq!(
            command.args,
            [
                "-p",
                "--mode",
                "json",
                "--no-session",
                "--no-title",
                "--max-cost=0.25",
                "--",
                "literal --prompt",
            ]
            .map(OsString::from)
        );
        assert_eq!(command.environment, harness.environment);
        for request in [
            HarnessRequest {
                model: Some("requested".into()),
                ..HarnessRequest::default()
            },
            HarnessRequest {
                resume: Some("session-ref".into()),
                ..HarnessRequest::default()
            },
        ] {
            assert_eq!(
                harness.plan(&request, &info).unwrap_err().kind,
                fabric_domain::ErrorKind::UnsupportedCapability
            );
        }
        for cost in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let request = HarnessRequest {
                max_cost_usd: Some(cost),
                ..HarnessRequest::default()
            };
            assert_eq!(
                harness.plan(&request, &info).unwrap_err().kind,
                fabric_domain::ErrorKind::InvalidArguments
            );
        }
        for version in ["omp/19.1.1", "omp/19.1.0+build", "omp/19.1.0-fixture+exact"] {
            assert_eq!(
                harness
                    .capabilities(&HarnessInfo {
                        version: version.into(),
                    })
                    .unwrap_err()
                    .kind,
                fabric_domain::ErrorKind::UnsupportedVersion
            );
        }
    }

    #[test]
    fn probe_rejects_failed_non_omp_multiline_and_oversized_versions() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("omp");
        let harness = OmpHarness {
            binary: binary.clone(),
            environment: Vec::new(),
        };
        for body in [
            "printf 'omp/19.1.0\\n'; exit 7",
            "printf 'other/19.1.0\\n'",
            "printf 'omp/19.1.0\\nextra\\n'",
            "printf 'omp/'; head -c 5000 /dev/zero | tr '\\000' x",
        ] {
            fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(harness.probe().is_err(), "{body}");
        }
    }

    #[test]
    fn command_rejects_non_utf8_and_nul_without_starting_an_executable() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = temp.path().join("prompt");
        let harness = OmpHarness {
            binary: "/must/not/run".into(),
            environment: Vec::new(),
        };
        for bytes in [b"invalid \xff".as_slice(), b"nul \0".as_slice()] {
            fs::write(&prompt, bytes).unwrap();
            assert!(
                harness
                    .command(&AgentSpec {
                        prompt_file: &prompt,
                        max_cost_usd: None,
                    })
                    .is_err()
            );
        }
    }
}
