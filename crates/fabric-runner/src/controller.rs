//! Explicit OMP controller bindings. No ambient environment, installation or auth copying.
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use fabric_domain::{AgentProfile, FabricError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::plugin::{PluginIntegrity, PluginRegistry};
use crate::profile::invalid;

pub const REFERENCE: &str = "omp-controller";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Controller {
    pub binary: PinnedFile,
    pub extension: PinnedFile,
    pub root: PathBuf,
    pub registry: PathBuf,
    pub plugin: Option<PluginIntegrity>,
}

pub fn file_sha256(path: &Path) -> Result<String, FabricError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| invalid("Controller pin must name a readable canonical regular file."))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid("Cannot inspect controller pin."))?;
    if !metadata.is_file()
        || metadata.len() > 128 * 1024 * 1024
        || path.canonicalize().ok().as_deref() != Some(path)
    {
        return Err(invalid(
            "Controller pin must name a canonical regular file of at most 128 MiB.",
        ));
    }
    let mut hash = Sha256::new();
    let mut count = 0;
    let mut buffer = [0u8; 8192];
    loop {
        let size = file
            .read(&mut buffer)
            .map_err(|_| invalid("Cannot hash controller pin."))?;
        if size == 0 {
            break;
        }
        count += size as u64;
        if count > 128 * 1024 * 1024 {
            return Err(invalid("Controller pin exceeds 128 MiB."));
        }
        hash.update(&buffer[..size]);
    }
    let after = file
        .metadata()
        .map_err(|_| invalid("Cannot recheck controller pin."))?;
    if count != metadata.len()
        || metadata.len() != after.len()
        || metadata.mtime() != after.mtime()
        || metadata.mtime_nsec() != after.mtime_nsec()
        || metadata.ctime() != after.ctime()
        || metadata.ctime_nsec() != after.ctime_nsec()
    {
        return Err(invalid("Controller pin changed during inspection."));
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl Controller {
    pub fn verify(&self) -> Result<(), FabricError> {
        for pin in [&self.binary, &self.extension] {
            if file_sha256(&pin.path)? != pin.sha256 {
                return Err(invalid(
                    "Controller binary/extension pin changed; review and run fabric up again.",
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(&self.root)
            .map_err(|_| invalid("Controller Fabric root is unavailable."))?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || self.root.canonicalize().ok().as_ref() != Some(&self.root)
        {
            return Err(invalid(
                "Controller Fabric root must be canonical and owner-only.",
            ));
        }
        PluginRegistry::load(&self.registry)?;
        if let Some(plugin) = &self.plugin {
            plugin.verify()?;
        }
        Ok(())
    }

    pub fn args(&self) -> Vec<OsString> {
        // Disable ambient extension discovery; explicit plugin/extension loading still works.
        let mut args = vec![
            "--no-extensions".into(),
            "-e".into(),
            self.extension.path.as_os_str().to_owned(),
        ];
        if let Some(plugin) = &self.plugin {
            args.extend(["--plugin-dir".into(), plugin.root.as_os_str().to_owned()]);
        }
        args
    }

    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        let mut environment = vec![
            (
                "FABRIC_BINARY".into(),
                self.binary.path.as_os_str().to_owned(),
            ),
            ("FABRIC_ROOT".into(), self.root.as_os_str().to_owned()),
            (
                "FABRIC_PLUGIN_REGISTRY".into(),
                self.registry.as_os_str().to_owned(),
            ),
        ];
        if let Some(plugin) = &self.plugin {
            environment.push((
                "FABRIC_CONTROLLER_PLUGIN".into(),
                plugin.root.as_os_str().to_owned(),
            ));
        }
        environment
    }
}

pub fn load(profile: &AgentProfile) -> Result<Option<Controller>, FabricError> {
    let mut references = profile
        .native_config_refs
        .iter()
        .filter(|r| r.kind == REFERENCE);
    let Some(reference) = references.next() else {
        return Ok(None);
    };
    if profile.harness_id.as_str() != "omp" || references.next().is_some() {
        return Err(invalid(
            "Only OMP supports one explicit controller reference.",
        ));
    }
    let directory = std::fs::symlink_metadata(&reference.path)
        .map_err(|_| invalid("Controller selection is unavailable; run fabric up again."))?;
    if !directory.is_dir()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o077 != 0
        || reference.path.canonicalize().ok().as_ref() != Some(&reference.path)
    {
        return Err(invalid(
            "Controller selection must be a canonical owner-only directory.",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(reference.path.join("controller.json"))
        .map_err(|_| invalid("Controller selection is unreadable."))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid("Controller selection is unreadable."))?;
    if !metadata.is_file()
        || metadata.len() > 64 * 1024
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(invalid(
            "Controller selection must be a private regular file of at most 64 KiB.",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("Controller selection is unreadable."))?;
    let controller: Controller =
        serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid controller selection."))?;
    controller.verify()?;
    Ok(Some(controller))
}

/// Bind controller content and selected executable/plugin identities into native provenance.
pub fn fingerprint(profile: &AgentProfile, base: String) -> Result<String, FabricError> {
    let Some(controller) = load(profile)? else {
        return Ok(base);
    };
    let mut hash = Sha256::new();
    hash.update(b"fabric-omp-controller-v1\0");
    hash.update(base);
    hash.update(
        serde_json::to_vec(&controller).map_err(|_| invalid("Invalid controller selection."))?,
    );
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pinned_controller_rejects_tampering_symlinks_and_duplicate_refs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for name in ["storage", "registry", "selection"] {
            std::fs::create_dir(root.join(name)).unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        PluginRegistry::load(&root.join("registry"))
            .unwrap()
            .save(&root.join("registry"))
            .unwrap();
        let path = root.join("native");
        std::fs::write(&path, b"pinned fixture").unwrap();
        let pin = PinnedFile {
            sha256: file_sha256(&path).unwrap(),
            path: path.clone(),
        };
        let binding = Controller {
            binary: pin.clone(),
            extension: pin,
            root: root.join("storage"),
            registry: root.join("registry"),
            plugin: None,
        };
        let selection = root.join("selection/controller.json");
        std::fs::write(&selection, serde_json::to_vec(&binding).unwrap()).unwrap();
        std::fs::set_permissions(&selection, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut profile = AgentProfile {
            harness_id: "omp".parse().unwrap(),
            harness_version: "omp/19.1.0".into(),
            native_config_refs: vec![fabric_domain::NativeConfigRef {
                kind: REFERENCE.into(),
                path: root.join("selection"),
            }],
            model_policy: fabric_domain::ModelPolicy::NativeDefault {},
            skills: vec![],
            plugins: vec![],
            mcp: vec![],
            connection_refs: vec![],
        };
        assert!(load(&profile).unwrap().is_some());
        assert_eq!(binding.environment().len(), 3);
        assert_ne!(fingerprint(&profile, "native".into()).unwrap(), "native");
        profile
            .native_config_refs
            .push(profile.native_config_refs[0].clone());
        assert!(load(&profile).is_err());
        profile.native_config_refs.pop();
        std::fs::hard_link(&selection, root.join("alias")).unwrap();
        assert!(load(&profile).is_err());
        std::fs::remove_file(root.join("alias")).unwrap();
        std::fs::write(&path, b"changed fixture").unwrap();
        assert!(load(&profile).is_err());
        std::os::unix::fs::symlink(&path, root.join("link")).unwrap();
        assert!(file_sha256(&root.join("link")).is_err());
    }
}
