use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use fabric_domain::{ErrorKind, FabricError, HarnessId};
use fabric_runner::NativeContext;
use serde::Serialize;

#[derive(clap::Subcommand)]
pub enum HarnessCommand {
    /// Show exact exposure and ask permission once; --yes is explicit automation consent.
    Connect {
        id: HarnessId,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove the stored native-context selection (idempotent).
    Revoke {
        id: HarnessId,
        #[arg(long)]
        json: bool,
    },
}
impl HarnessCommand {
    pub fn json(&self) -> bool {
        match self {
            Self::Connect { json, .. } | Self::Revoke { json, .. } => *json,
        }
    }
}

pub(super) fn invalid(message: impl Into<String>) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidArguments,
        message: message.into(),
    }
}
pub(super) fn approve(question: &str) -> Result<bool, FabricError> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{question} [y/N] ");
    io::stderr().flush().map_err(super::output_error)?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(super::output_error)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}
pub(super) fn exposure(context: &NativeContext) {
    eprintln!(
        "Harness: {} {}",
        context.profile.harness_id, context.profile.harness_version
    );
    for reference in &context.profile.native_config_refs {
        eprintln!("Native {}: {}", reference.kind, reference.path.display());
    }
    for path in &context.credential_paths {
        eprintln!("Credential reference: {}", path.display());
    }
    eprintln!(
        "Exposure: {}\nTrusted-local execution is NOT a security sandbox. The harness owns authentication; Fabric does not read or copy credentials.",
        context.exposure
    );
}
fn connect(root: &Path, context: &NativeContext, yes: bool, json: bool) -> Result<(), FabricError> {
    if !json {
        exposure(context);
    }
    if !yes && (json || !approve("Consent to use exactly this native context?")?) {
        return Err(invalid(
            "Native-context consent was not granted; use `fabric harness connect <id>` interactively or --yes for explicit automation consent.",
        ));
    }
    fabric_core::onboarding::connect(root, context)
}
pub fn harness(root: Option<PathBuf>, command: HarnessCommand) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    match command {
        HarnessCommand::Connect { id, yes, json } => {
            let context = fabric_core::onboarding::inspect(&id)?;
            connect(&root, &context, yes, json)?;
            super::write_inspection(
                &serde_json::json!({"harness_id": id, "connected": true, "context": context}),
                json,
            )
        }
        HarnessCommand::Revoke { id, json } => {
            fabric_core::onboarding::revoke(&root, &id)?;
            super::write_inspection(
                &serde_json::json!({"harness_id": id, "connected": false}),
                json,
            )
        }
    }
}

#[derive(Serialize)]
struct InitReport {
    source: String,
    source_commit_sha: String,
    project_name: String,
    dirty: bool,
    harness_id: Option<HarnessId>,
    version: Option<String>,
    credential_available: bool,
    connected: bool,
    ready: bool,
    native_context: Option<NativeContext>,
    discovery_errors: Vec<String>,
    likely_recipe: Option<String>,
    actions: Vec<String>,
}

fn write_init(report: &InitReport, json: bool) -> Result<(), FabricError> {
    if json {
        return super::write_inspection(report, true);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "Project: {}\nSource: {}\nCommit: {}\nDirty: {} (only committed HEAD is prepared)\nREADY: {}\nConnected: {}\nNative authentication available: {}",
        report.project_name, report.source, report.source_commit_sha, report.dirty, report.ready, report.connected, report.credential_available).map_err(super::output_error)?;
    if let Some(context) = &report.native_context {
        writeln!(
            out,
            "Harness: {} {}\nExposure: {}",
            context.profile.harness_id, context.profile.harness_version, context.exposure
        )
        .map_err(super::output_error)?;
        for reference in &context.profile.native_config_refs {
            writeln!(
                out,
                "Native {}: {}",
                reference.kind,
                reference.path.display()
            )
            .map_err(super::output_error)?;
        }
        for path in &context.credential_paths {
            writeln!(out, "Credential reference: {}", path.display())
                .map_err(super::output_error)?;
        }
    } else {
        writeln!(
            out,
            "Harness: none installed; install a supported harness and retry."
        )
        .map_err(super::output_error)?;
    }
    writeln!(
        out,
        "Likely recipe: {}",
        report
            .likely_recipe
            .as_deref()
            .unwrap_or("none inferred; provide a reviewed recipe")
    )
    .map_err(super::output_error)?;
    for action in &report.actions {
        writeln!(out, "Next: {action}").map_err(super::output_error)?;
    }
    for error in &report.discovery_errors {
        writeln!(out, "Discovery: {error}").map_err(super::output_error)?;
    }
    out.flush().map_err(super::output_error)
}

pub fn init(
    root: Option<PathBuf>,
    id: Option<HarnessId>,
    check: bool,
    json: bool,
) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let cwd = std::env::current_dir().map_err(super::output_error)?;
    let source = fabric_project::resolve_source(&cwd)?;
    let path = Path::new(&source.path);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("Git source needs a UTF-8 project name."))?
        .to_owned();
    let mut errors = Vec::new();
    let context = if let Some(id) = id {
        Some(fabric_core::onboarding::inspect(&id)?)
    } else {
        let mut selected = None;
        for id in fabric_runner::HarnessRegistry::default().ids()? {
            match fabric_core::onboarding::inspect(&id) {
                Ok(context) => {
                    selected = Some(context);
                    break;
                }
                Err(error) if error.kind == ErrorKind::UnsupportedCapability => {}
                Err(error) => errors.push(error.message),
            }
        }
        selected
    };
    let connected = match &context {
        Some(context) => {
            fabric_core::onboarding::load(&root, &context.profile.harness_id)?.is_some()
        }
        None => false,
    };
    let registered = match fabric_core::inspect::project(&root, &name) {
        Ok(report) if report.project.source_path == source.path => Some(report),
        Ok(_) => {
            return Err(invalid(
                "This project name is registered to a different source; use an explicit `fabric project add --name <unique-name> --path <source>`.",
            ));
        }
        Err(error) if error.kind == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let ready = registered.as_ref().is_some_and(|report| {
        report.prepared_states.iter().any(|state| {
            state.source_commit_sha == source.commit_sha
                && state.status == fabric_domain::PreparedStateStatus::Ready
        })
    });
    let existing = ["fabric.recipe.toml", ".fabric/recipe.toml"]
        .into_iter()
        .map(|name| path.join(name))
        .find(|path| path.is_file());
    let suggestion = if path.join("Cargo.toml").is_file() {
        Some((
            "Rust: cargo build --locked",
            "version = 1\n[[steps]]\nargv = [\"cargo\", \"build\", \"--locked\"]\n",
        ))
    } else if path.join("package-lock.json").is_file() {
        Some((
            "npm lockfile: npm ci",
            "version = 1\n[[steps]]\nargv = [\"npm\", \"ci\"]\n",
        ))
    } else {
        None
    };
    let mut report = InitReport {
        source: source.path.clone(), source_commit_sha: source.commit_sha.clone(), project_name: name.clone(), dirty: source.dirty,
        harness_id: context.as_ref().map(|c| c.profile.harness_id.clone()), version: context.as_ref().map(|c| c.profile.harness_version.clone()),
        credential_available: context.as_ref().is_some_and(|c| c.credential_available), connected, ready,
        native_context: context, discovery_errors: errors,
        likely_recipe: existing.as_ref().map(|p| p.display().to_string()).or_else(|| suggestion.map(|s| s.0.to_owned())),
        actions: vec!["Connect a native context explicitly; authentication remains owned by the harness.".into(), "Review a recipe before explicitly approving preparation; only committed HEAD is prepared.".into()],
    };
    if check || json || !io::stdin().is_terminal() {
        return write_init(&report, json);
    }
    write_init(&report, false)?;
    let context = report.native_context.as_ref().ok_or_else(|| invalid("No supported installed harness found; install a harness and retry `fabric init --harness <id>`."))?;
    if !report.connected {
        connect(&root, context, false, false)?;
        report.connected = true;
    }
    if registered.is_none() {
        if !approve("Register this committed Git source with Fabric?")? {
            return Ok(());
        }
        fabric_project::register(&root, &name, path)?;
    }
    if !ready {
        let recipe = if let Some(existing) = existing {
            Some(existing)
        } else if let Some((label, text)) = suggestion {
            eprintln!("Suggested recipe ({label}):\n{text}");
            if approve(
                "Create this reviewed recipe in private Fabric storage (not the source checkout)?",
            )? {
                let destination = root.join(format!("init-{name}.recipe.toml"));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&destination)
                    .map_err(super::output_error)?;
                file.write_all(text.as_bytes())
                    .map_err(super::output_error)?;
                Some(destination)
            } else {
                None
            }
        } else {
            eprintln!(
                "No safe recipe inferred. Supply a reviewed recipe with `fabric project prepare {name} --recipe <path>`."
            );
            None
        };
        if let Some(recipe) = recipe {
            let file = std::fs::File::open(&recipe).map_err(super::output_error)?;
            if !file.metadata().map_err(super::output_error)?.is_file() {
                return Err(invalid(
                    "Recipe must be a regular UTF-8 file of at most 64 KiB.",
                ));
            }
            let mut text = String::new();
            file.take(64 * 1024 + 1)
                .read_to_string(&mut text)
                .map_err(super::output_error)?;
            if text.len() > 64 * 1024 {
                return Err(invalid("Recipe must be at most 64 KiB."));
            }
            fabric_project::recipe::Recipe::parse(&text)?;
            eprintln!("Review {}:\n{text}", recipe.display());
            if approve("Run exactly this recipe to prepare committed HEAD?")? {
                fabric_core::workspace::storage::prepare(
                    &root,
                    &name,
                    &recipe,
                    &Default::default(),
                )?;
                report.ready = true;
            }
        }
    }
    write_init(&report, false)
}
