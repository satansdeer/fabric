//! Adapter construction and harmless executable-discovery metadata.
use std::collections::HashMap;
use std::path::PathBuf;

use fabric_domain::{ErrorKind, FabricError, HarnessId};

use crate::{FakeAHarness, FakeBHarness, FakeHarness, HarnessAdapter};

#[derive(Default)]
pub struct RegistryOptions {
    pub fake_binary: Option<PathBuf>,
}

pub struct HarnessRegistration {
    pub id: HarnessId,
    pub doctor_executable: Option<&'static str>,
    pub create: fn(&RegistryOptions) -> Result<Box<dyn HarnessAdapter>, FabricError>,
}

pub struct HarnessRegistry {
    entries: HashMap<HarnessId, HarnessRegistration>,
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn register(&mut self, registration: HarnessRegistration) -> Result<(), FabricError> {
        if self.entries.contains_key(&registration.id) {
            return Err(FabricError {
                kind: ErrorKind::InvalidArguments,
                message: format!("Harness {} is already registered.", registration.id),
            });
        }
        self.entries.insert(registration.id.clone(), registration);
        Ok(())
    }

    pub fn resolve(
        &self,
        id: &HarnessId,
        options: &RegistryOptions,
    ) -> Result<Box<dyn HarnessAdapter>, FabricError> {
        if !self.entries.contains_key(id) {
            if options.fake_binary.is_some() {
                return Err(FabricError {
                    kind: ErrorKind::InvalidArguments,
                    message: "External plugins do not accept --fake-binary.".into(),
                });
            }
            if let Some(registry) = crate::plugin::configured_registry()? {
                if registry.plugins.iter().any(|p| p.manifest.id == *id) {
                    let manifest = registry.select(id, crate::plugin::PluginKind::Harness)?;
                    return Ok(Box::new(crate::plugin::PluginHarness::new(manifest)?));
                }
            }
        }
        let registration = self.entries.get(id).ok_or_else(|| FabricError {
            kind: ErrorKind::UnknownHarness,
            message: format!("Unknown harness {id}; explicitly install a content-pinned harness plugin and set FABRIC_PLUGIN_REGISTRY."),
        })?;
        let adapter = (registration.create)(options)?;
        if adapter.id() != *id {
            return Err(FabricError {
                kind: ErrorKind::InvalidArguments,
                message: format!("Registered adapter identity does not match {id}."),
            });
        }
        Ok(adapter)
    }

    pub fn ids(&self) -> Result<Vec<HarnessId>, FabricError> {
        let mut ids: Vec<_> = self.entries.keys().cloned().collect();
        if let Some(registry) = crate::plugin::configured_registry()? {
            ids.extend(
                registry
                    .plugins
                    .into_iter()
                    .filter(|p| !p.revoked && p.manifest.kind == crate::plugin::PluginKind::Harness)
                    .map(|p| p.manifest.id),
            );
        }
        ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        ids.dedup();
        Ok(ids)
    }

    pub fn doctor_executables(&self) -> Result<Vec<String>, FabricError> {
        let mut names: Vec<_> = self
            .entries
            .values()
            .filter_map(|entry| entry.doctor_executable.map(str::to_owned))
            .collect();
        if let Some(registry) = crate::plugin::configured_registry()? {
            names.extend(
                registry
                    .plugins
                    .into_iter()
                    .filter(|p| !p.revoked && p.manifest.kind == crate::plugin::PluginKind::Harness)
                    .filter_map(|p| p.manifest.native.map(|n| n.executable)),
            );
        }
        names.sort_unstable();
        names.dedup();
        Ok(names)
    }
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        for registration in [
            HarnessRegistration {
                id: "fake".parse().expect("registered harness ID"),
                doctor_executable: None,
                create: |options| {
                    let binary = match &options.fake_binary {
                        Some(binary) => binary.clone(),
                        None => std::env::current_exe()
                            .map_err(crate::execution_error)?
                            .with_file_name("fabric-fake-harness"),
                    };
                    Ok(Box::new(FakeHarness::new(binary)?))
                },
            },
            HarnessRegistration {
                id: "fake-a".parse().expect("registered harness ID"),
                doctor_executable: None,
                create: |options| {
                    let binary = match &options.fake_binary {
                        Some(binary) => binary.clone(),
                        None => std::env::current_exe()
                            .map_err(crate::execution_error)?
                            .with_file_name("fabric-fake-harness"),
                    };
                    Ok(Box::new(FakeAHarness::new(binary)?))
                },
            },
            HarnessRegistration {
                id: "fake-b".parse().expect("registered harness ID"),
                doctor_executable: None,
                create: |options| {
                    let binary = match &options.fake_binary {
                        Some(binary) => binary.clone(),
                        None => std::env::current_exe()
                            .map_err(crate::execution_error)?
                            .with_file_name("fabric-fake-harness"),
                    };
                    Ok(Box::new(FakeBHarness::new(binary)?))
                },
            },
        ] {
            registry.register(registration).expect("unique built-in ID");
        }
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_accepts_a_non_builtin_identity_and_rejects_unknown_ids() {
        struct Custom;
        impl HarnessAdapter for Custom {
            fn id(&self) -> HarnessId {
                "test-worker".parse().unwrap()
            }
            fn probe(&self) -> Result<crate::HarnessInfo, FabricError> {
                Ok(crate::HarnessInfo {
                    version: "test-worker 1".into(),
                })
            }
            fn command(&self, _: &crate::AgentSpec<'_>) -> Result<crate::CommandSpec, FabricError> {
                Err(crate::execution_error("No test invocation requested"))
            }
        }
        let mut registry = HarnessRegistry::default();
        let id = "test-worker".parse().unwrap();
        registry
            .register(HarnessRegistration {
                id,
                doctor_executable: None,
                create: |_| Ok(Box::new(Custom)),
            })
            .unwrap();
        let id = "test-worker".parse().unwrap();
        let adapter = registry.resolve(&id, &RegistryOptions::default()).unwrap();
        assert_eq!(adapter.id(), id);
        assert_eq!(adapter.probe().unwrap().version, "test-worker 1");
        let failure = registry
            .resolve(
                &"unknown-worker".parse().unwrap(),
                &RegistryOptions::default(),
            )
            .err()
            .unwrap();
        assert_eq!(failure.kind, ErrorKind::UnknownHarness);
        assert!(registry.doctor_executables().unwrap().is_empty());
    }
}
