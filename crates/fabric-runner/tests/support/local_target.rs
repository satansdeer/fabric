use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Content-pinned reference target installation for lifecycle/recovery tests.
pub fn install(root: &Path, binary: &Path) -> PathBuf {
    let registry = root.canonicalize().unwrap().join("plugin-registry");
    if !registry.exists() {
        std::fs::create_dir(&registry).unwrap();
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let package = registry.join("local-target");
    std::fs::create_dir(&package).unwrap();
    std::fs::set_permissions(&package, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = package.join("plugin");
    std::fs::copy(binary, &executable).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let content = Sha256::digest(std::fs::read(&executable).unwrap());
    let pin = format!("{:x}", Sha256::digest(format!("plugin\0{content:x}\n")));
    let manifest = serde_json::from_value(serde_json::json!({
        "id":"local","version":"0.1.0","kind":"target","protocol":{"min":1,"max":1},
        "executable":executable,"args":[],"capabilities":["target.execute"],
        "integrity":{"root":package,"sha256":pin}
    }))
    .unwrap();
    let mut installed = fabric_runner::plugin::PluginRegistry::load(&registry).unwrap();
    installed.install(manifest, false).unwrap();
    installed.save(&registry).unwrap();
    executable
}
