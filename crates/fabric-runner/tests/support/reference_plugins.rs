use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Explicit, content-pinned installation of the separately built SDK executable.
pub fn install(root: &Path) -> PathBuf {
    let registry = root.canonicalize().unwrap().join("plugin-registry");
    if registry.exists() {
        return registry;
    }
    let binary = std::env::var_os("FABRIC_REFERENCE_PLUGIN_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("fabric-reference-harnesses")
        });
    assert!(
        binary.is_file(),
        "Build SDK reference executable first; set FABRIC_REFERENCE_PLUGIN_BINARY to its absolute path: {}",
        binary.display()
    );
    std::fs::create_dir(&registry).unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
    let package = registry.join("reference");
    std::fs::create_dir(&package).unwrap();
    std::fs::set_permissions(&package, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = package.join("plugin");
    std::fs::copy(binary, &executable).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let content = Sha256::digest(std::fs::read(&executable).unwrap());
    let pin = format!("{:x}", Sha256::digest(format!("plugin\0{content:x}\n")));
    let mut installed = fabric_runner::plugin::PluginRegistry::default();
    for (id, roots, fingerprint) in [
        (
            "omp",
            vec!["PI_CODING_AGENT_DIR"],
            serde_json::json!({"domain":"omp-native-config-v1", "files":["config.yml","config.yaml","settings.json","models.yml","models.yaml","mcp.json",".mcp.json"], "roots":[["native-agent-dir",""],["native-home",".omp/agent"]]}),
        ),
        (
            "codex",
            vec!["CODEX_HOME"],
            serde_json::json!({"domain":"codex-native-config-v1", "files":["config.toml"], "roots":[["native-config-dir",""]]}),
        ),
    ] {
        let manifest = serde_json::from_value(serde_json::json!({
            "id":id,"version":"0.1.0","kind":"harness","protocol":{"min":1,"max":1},
            "executable":executable,"args":[id],"capabilities":["harness.run"],
            "integrity":{"root":package,"sha256":pin},
            "native":{"executable":id,"root_environment":roots,"fingerprint":fingerprint}
        }))
        .unwrap();
        installed.install(manifest, false).unwrap();
    }
    installed.save(&registry).unwrap();
    registry
}

/// Use the same installed target boundary for local lifecycle and recovery tests.
pub fn install_local(root: &Path, binary: &Path) -> PathBuf {
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
    })).unwrap();
    let mut installed = fabric_runner::plugin::PluginRegistry::load(&registry).unwrap();
    installed.install(manifest, false).unwrap();
    installed.save(&registry).unwrap();
    executable
}

pub fn local_pin(root: &Path) -> fabric_runner::target::TargetPin {
    let registry = root.canonicalize().unwrap().join("plugin-registry");
    let manifest = fabric_runner::plugin::PluginRegistry::load(&registry).unwrap()
        .select(&"local".parse().unwrap(), fabric_runner::plugin::PluginKind::Target).unwrap();
    let mut pin = fabric_runner::target::TargetPin::from_manifest(manifest, "local").unwrap();
    pin.registry = Some(registry);
    pin
}
