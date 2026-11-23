//! One setup path, using the existing registry, native consent and preparation cache.
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use fabric_domain::{FabricError, HarnessId, HarnessRequest, NativeConfigRef};
use fabric_runner::controller::{Controller, PinnedFile, file_sha256};
use fabric_runner::plugin::{
    PluginIntegrity, PluginKind, PluginManifest, PluginRegistry, ProtocolRange,
};
use fabric_runner::{AgentSpec, HarnessRegistry, RegistryOptions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::onboarding::{approve, exposure, invalid};

#[derive(clap::Args)]
pub struct UpArgs {
    #[arg(long)]
    harness: Option<HarnessId>,
    /// Enable isolated packaged OMP tools; native user configuration is never edited.
    #[arg(long)]
    controller: bool,
    /// Explicit reviewed plugin/skill tree (private, canonical, content pinned).
    #[arg(long, requires_all = ["controller", "plugin_sha256"])]
    plugin_dir: Option<PathBuf>,
    #[arg(long, requires = "plugin_dir")]
    plugin_sha256: Option<String>,
    /// Reviewed preparation recipe; otherwise use the repo recipe or a detected lockfile.
    #[arg(long, conflicts_with = "source_only")]
    recipe: Option<PathBuf>,
    /// Explicitly prepare committed source without installing dependencies.
    #[arg(long)]
    source_only: bool,
    /// Explicit trust/execute/auth/preparation consent for unattended setup.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    pub json: bool,
}

fn consent(yes: bool, question: &str) -> Result<(), FabricError> {
    if yes || approve(question)? {
        Ok(())
    } else {
        Err(invalid(
            "Consent not granted. Review the displayed operations, then use interactive `fabric up` or explicit --yes.",
        ))
    }
}
fn read(path: &Path) -> Result<Vec<u8>, FabricError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(super::output_error)?;
    if !file.metadata().map_err(super::output_error)?.is_file() {
        return Err(invalid("Setup metadata/recipe must be a regular file."));
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(super::output_error)?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("Setup metadata/recipe exceeds 1 MiB."));
    }
    Ok(bytes)
}
fn private_dir(path: &Path) -> Result<(), FabricError> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(path)
        .map_err(super::output_error)
}
fn registry_environment(path: &Path) {
    // CLI setup/launch is single-threaded; no concurrent environment readers exist here.
    unsafe {
        std::env::set_var("FABRIC_PLUGIN_REGISTRY", path);
    }
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn bundled_plugins(bundle: &Path, release: &Value, registry: &Path) -> Result<(), FabricError> {
    let mut installed = PluginRegistry::load(registry)?;
    for (list, kind, capability) in [
        ("plugins", PluginKind::Harness, "harness.run"),
        ("target_plugins", PluginKind::Target, "target.execute"),
    ] {
        for pin in release[list]
            .as_array()
            .ok_or_else(|| invalid("Release plugin manifest is missing."))?
        {
            let id = pin["id"]
                .as_str()
                .ok_or_else(|| invalid("Release plugin ID is missing."))?;
            // These are the only packages in the native macOS arm64 distribution.
            if !matches!(id, "omp" | "codex" | "local") {
                return Err(invalid("Unsupported bundled package."));
            }
            let package = bundle.join("plugins").join(id);
            let metadata: Value = serde_json::from_slice(&read(&package.join("package.json"))?)
                .map_err(super::output_error)?;
            let plugin = &metadata["fabricPlugin"];
            if plugin["runtime"] != "native"
                || plugin["entry"] != "plugin"
                || plugin["id"] != id
                || plugin["protocol"] != 1
            {
                return Err(invalid(
                    "Bundle requires a native protocol-1 package; reinstall the official release.",
                ));
            }
            let manifest = PluginManifest {
                id: id.parse()?,
                version: metadata["version"]
                    .as_str()
                    .ok_or_else(|| invalid("Package version missing."))?
                    .into(),
                protocol: ProtocolRange { min: 1, max: 1 },
                kind,
                executable: package.join("plugin"),
                args: serde_json::from_value(plugin.get("args").cloned().unwrap_or(json!([])))
                    .map_err(super::output_error)?,
                capabilities: vec![capability.into()],
                integrity: Some(PluginIntegrity {
                    root: package,
                    sha256: pin["sha256"]
                        .as_str()
                        .ok_or_else(|| invalid("Package content pin missing."))?
                        .into(),
                }),
                native: plugin
                    .get("native")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(super::output_error)?,
            };
            if let Some(existing) = installed
                .plugins
                .iter()
                .find(|p| p.manifest.id == manifest.id)
            {
                if existing.revoked
                    || serde_json::to_value(&existing.manifest).ok()
                        != serde_json::to_value(&manifest).ok()
                {
                    return Err(invalid(
                        "Existing bundled plugin selection differs or is revoked; review it with fabric plugins, or choose a fresh private registry.",
                    ));
                }
                manifest.verify_integrity()?;
            } else {
                installed.install(manifest, false)?;
            }
        }
    }
    installed.save(registry)
}

pub fn up(root: Option<PathBuf>, args: UpArgs) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .map_err(super::output_error)?
            .join(root)
    };
    let source =
        fabric_project::resolve_source(&std::env::current_dir().map_err(super::output_error)?)?;
    let path = Path::new(&source.path);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("Git source needs a UTF-8 project name."))?;
    if args.controller && args.harness.as_ref().is_some_and(|id| id.as_str() != "omp") {
        return Err(invalid(
            "--controller requires the native OMP adapter (omp/19.1.0).",
        ));
    }
    let binary = std::env::current_exe()
        .map_err(super::output_error)?
        .canonicalize()
        .map_err(super::output_error)?;
    let bundle = binary
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("Cannot locate release bundle."))?;
    let release = if bundle.join("release.json").is_file() {
        Some(
            serde_json::from_slice::<Value>(&read(&bundle.join("release.json"))?)
                .map_err(super::output_error)?,
        )
    } else {
        None
    };
    let registry = std::env::var_os("FABRIC_PLUGIN_REGISTRY")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("up-registry"));
    eprintln!(
        "Source: {}\nCommit: {}\nDirty: {} (only committed HEAD is prepared)\nStorage: {}\nRegistry: {}",
        source.path,
        source.commit_sha,
        source.dirty,
        root.display(),
        registry.display()
    );
    eprintln!(
        "Setup may execute selected installed harness/plugin version and native-context probes. Subsequent consent covers native authentication access and preparation commands. Trusted local code, NOT a sandbox; no credentials copied, no ambient install, no model turn during setup."
    );
    if args.controller {
        eprintln!(
            "Controller opt-in: exact binary {}, packaged Fabric typed extension; ambient extension discovery disabled. External plugin: {:?}, SHA256 {:?}. Native provider/model/effort/auth remain selected by OMP.",
            binary.display(),
            args.plugin_dir,
            args.plugin_sha256
        );
    }
    consent(
        args.yes,
        "Trust/execute the selected local harness and official bundled plugins for setup?",
    )?;
    fabric_project::initialize(&root)?;
    let root = root.canonicalize().map_err(super::output_error)?;
    if let Some(release) = &release {
        if release["platform"] != "macos-arm64"
            || !cfg!(all(target_os = "macos", target_arch = "aarch64"))
        {
            return Err(invalid(
                "Only the actually packaged macOS arm64 native distribution is supported.",
            ));
        }
        if release["files"]["bin/fabric"].as_str() != Some(file_sha256(&binary)?.as_str()) {
            return Err(invalid(
                "Release binary content pin differs; reinstall the reviewed bundle.",
            ));
        }
        private_dir(&registry)?;
        bundled_plugins(bundle, release, &registry)?;
    } else if args.controller {
        return Err(invalid(
            "Controller setup requires a native release bundle with release.json and packaged integrations/omp/fabric.ts; a standalone development binary is insufficient.",
        ));
    }
    PluginRegistry::load(&registry)?;
    let registry = registry.canonicalize().map_err(super::output_error)?;
    registry_environment(&registry);
    let ids = if let Some(id) = args.harness {
        vec![id]
    } else if args.controller {
        vec!["omp".parse()?]
    } else {
        HarnessRegistry::default().ids()?
    };
    let mut selected = None;
    let mut errors = Vec::new();
    for id in ids {
        match fabric_core::onboarding::inspect(&id) {
            Ok(context) if context.credential_available => { selected = Some(context); break; }
            Ok(_) => errors.push(format!("{id}: native authentication metadata is missing; configure/login with the harness locally")),
            Err(error) => errors.push(format!("{id}: {}", error.message)),
        }
    }
    let mut context = selected.ok_or_else(|| invalid(format!("No supported installed native harness/config found. Install OMP 19.1.0 or Codex 0.171.0 and configure authentication locally, then retry --harness <id>. {}", errors.join("; "))))?;
    exposure(&context);
    if !context.credential_available {
        return Err(invalid(
            "Selected native auth metadata is unavailable; configure/login with the harness locally, then retry. Fabric does not install a harness or copy credentials.",
        ));
    }
    let controller = if args.controller {
        let release = release.as_ref().unwrap();
        let extension = bundle.join("integrations/omp/fabric.ts");
        let pin = release["omp_extension"]["sha256"]
            .as_str()
            .ok_or_else(|| invalid("Release extension pin missing."))?;
        Some(Controller {
            binary: PinnedFile {
                path: binary.clone(),
                sha256: file_sha256(&binary)?,
            },
            extension: PinnedFile {
                path: extension,
                sha256: pin.into(),
            },
            root: root.clone(),
            registry: registry.clone(),
            plugin: args.plugin_dir.map(|root| PluginIntegrity {
                root,
                sha256: args.plugin_sha256.unwrap(),
            }),
        })
    } else {
        None
    };
    if let Some(controller) = &controller {
        controller.verify()?;
    }
    let recipe_path = args.recipe.or_else(|| {
        ["fabric.recipe.toml", ".fabric/recipe.toml"]
            .into_iter()
            .map(|file| path.join(file))
            .find(|file| file.is_file())
    });
    let recipe = if args.source_only {
        "version = 1\n[[steps]]\nargv = [\"/usr/bin/true\"]\n".to_owned()
    } else if let Some(recipe) = recipe_path {
        String::from_utf8(read(&recipe)?).map_err(super::output_error)?
    } else if path.join("Cargo.lock").is_file() && path.join("Cargo.toml").is_file() {
        "version = 1\nenvironment = [\"PATH\"]\n[[steps]]\nargv = [\"cargo\", \"build\", \"--locked\"]\n".to_owned()
    } else if path.join("package-lock.json").is_file() {
        "version = 1\nenvironment = [\"PATH\"]\n[[steps]]\nargv = [\"npm\", \"ci\"]\n".to_owned()
    } else {
        return Err(invalid(
            "No preparation requirements inferred. Use --source-only for a source-only tree, --recipe <reviewed-path>, or a repository fabric.recipe.toml. No dependency installation was attempted.",
        ));
    };
    let parsed = fabric_project::recipe::Recipe::parse(&recipe)?;
    let environment = parsed
        .environment()
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
        .collect();
    let fingerprint = parsed.fingerprint(&source.commit_sha, &environment)?;
    eprintln!(
        "Preparation (committed HEAD only; commands may access network/auth through the selected recipe):\n{recipe}"
    );
    consent(
        args.yes,
        "Approve exactly this native context, optional pinned controller/plugin execution, and preparation recipe?",
    )?;
    if let Some(controller) = &controller {
        // Immutable content-addressed selection; never overwrite a consented controller binding.
        let bytes = serde_json::to_vec(controller).map_err(super::output_error)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let selection = root.join(format!("controller-{digest}"));
        private_dir(&selection)?;
        let destination = selection.join("controller.json");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&destination)
        {
            Ok(mut file) => {
                file.write_all(&bytes).map_err(super::output_error)?;
                file.sync_all().map_err(super::output_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if read(&destination)? != bytes {
                    return Err(invalid("Existing controller binding differs."));
                }
            }
            Err(error) => return Err(super::output_error(error)),
        }
        context.profile.native_config_refs.push(NativeConfigRef {
            kind: fabric_runner::controller::REFERENCE.into(),
            path: selection,
        });
    }
    let previous = fabric_core::onboarding::load(&root, &context.profile.harness_id)?;
    let connected =
        serde_json::to_value(&previous).ok() == serde_json::to_value(Some(&context.profile)).ok();
    let valid = if connected {
        let adapter = HarnessRegistry::default()
            .resolve(&context.profile.harness_id, &RegistryOptions::default())?;
        fabric_core::onboarding::plan(
            &root,
            adapter.as_ref(),
            HarnessRequest::default(),
            &adapter.probe()?,
        )
        .is_ok()
    } else {
        false
    };
    if !valid {
        fabric_core::onboarding::connect(&root, &context)?;
    }
    let existing = match fabric_core::inspect::project(&root, name) {
        Ok(report) if report.project.source_path == source.path => Some(report),
        Ok(_) => {
            return Err(invalid(
                "Project name already refers to another source; select a different private root or register a unique project name explicitly.",
            ));
        }
        Err(error) if error.kind == fabric_domain::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if existing.is_none() {
        fabric_project::register(&root, name, path)?;
    }
    let recipe_file = root.join(format!("up-{}.recipe.toml", fingerprint.recipe_digest));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&recipe_file)
    {
        Ok(mut file) => file
            .write_all(recipe.as_bytes())
            .map_err(super::output_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read(&recipe_file)? != recipe.as_bytes() {
                return Err(invalid("Stored recipe differs from its content identity."));
            }
        }
        Err(error) => return Err(super::output_error(error)),
    }
    let ready =
        fabric_core::workspace::storage::prepare(&root, name, &recipe_file, &Default::default())?;
    let prefix = format!(
        "env FABRIC_PLUGIN_REGISTRY={} {} --root {}",
        quote(&registry.to_string_lossy()),
        quote(&binary.to_string_lossy()),
        quote(&root.to_string_lossy())
    );
    let next = if args.controller {
        format!("{prefix} controller")
    } else {
        format!(
            "{prefix} workers spawn --project {} --harness {} --prompt 'Describe the task' --json",
            quote(name),
            context.profile.harness_id
        )
    };
    super::write_inspection(
        &json!({"project": name, "source": source.path, "source_commit_sha": source.commit_sha, "prepared_state_id": ready.state_id, "preparation_key": ready.preparation_key, "harness_id": context.profile.harness_id, "controller": controller.is_some(), "next_command": next, "routing": "Use /fabric help for status/results/wait/cancel and registration details. /swarm explicitly routes local work through Fabric typed tools. No model turn has run."}),
        args.json,
    )
}

pub fn controller(root: Option<PathBuf>, rpc: bool) -> Result<(), FabricError> {
    let root = root
        .map_or_else(fabric_project::default_root, Ok)?
        .canonicalize()
        .map_err(super::output_error)?;
    let id = "omp".parse()?;
    let profile = fabric_core::onboarding::load(&root, &id)?.ok_or_else(|| {
        invalid("Run fabric up --controller first; explicit native consent is required.")
    })?;
    let binding = fabric_runner::controller::load(&profile)?
        .ok_or_else(|| invalid("No controller opt-in; run fabric up --controller."))?;
    if binding.root != root
        || binding.binary.path
            != std::env::current_exe()
                .map_err(super::output_error)?
                .canonicalize()
                .map_err(super::output_error)?
    {
        return Err(invalid(
            "Controller binary/storage differs from the explicitly consented binding.",
        ));
    }
    registry_environment(&binding.registry);
    let adapter = HarnessRegistry::default().resolve(&id, &RegistryOptions::default())?;
    let (plan, stamp) = fabric_core::onboarding::plan(
        &root,
        adapter.as_ref(),
        HarnessRequest::default(),
        &adapter.probe()?,
    )?;
    let prompt = tempfile::NamedTempFile::new().map_err(super::output_error)?;
    let native = adapter.command(&AgentSpec {
        profile: &plan.profile,
        prompt_file: prompt.path(),
        max_cost_usd: None,
        request: &plan.request,
    })?;
    let mut command = std::process::Command::new(native.program);
    command.env_clear();
    let _home = fabric_runner::profile::private_environment(&mut command)?;
    command.envs(native.environment).args(binding.args());
    if rpc {
        command.args(["--mode", "rpc", "--no-ui", "--no-session", "--no-title"]);
    }
    fabric_core::onboarding::revalidate(&root, &plan, stamp.as_deref())?;
    let status = command.status().map_err(super::output_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(invalid(format!("Native OMP controller exited {status}.")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn setup_metadata_rejects_symlinks_and_fifos() {
        let temp = tempfile::tempdir().unwrap();
        let plain = temp.path().join("recipe");
        std::fs::write(&plain, b"version = 1\n").unwrap();
        assert_eq!(read(&plain).unwrap(), b"version = 1\n");
        let link = temp.path().join("link");
        symlink(&plain, &link).unwrap();
        assert!(read(&link).is_err());
        let fifo = temp.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read(&fifo).is_err());
    }
}
