mod connect;
mod onboarding;
mod up;

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use fabric_domain::{
    AgentProfile, Capability, ErrorKind, FabricError, HarnessCapabilityManifest, HarnessId,
    HarnessRequest, SourceRef, WorkerTask,
};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "fabric",
    version = option_env!("FABRIC_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
    about = "Trusted-local agent workspaces"
)]
struct Cli {
    /// Private Fabric storage root (default: $XDG_DATA_HOME/fabric or ~/.local/share/fabric).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Consent once to local setup, preparation and optional OMP controller tools.
    Up(up::UpArgs),
    /// Maintain an authenticated outbound connection to a claimed generic Connect automation.
    Connect(connect::Options),
    #[command(hide = true)]
    ConnectAdmit {
        #[arg(long)]
        automation: PathBuf,
        #[arg(long)]
        fake_binary: Option<PathBuf>,
    },
    /// Launch the explicitly connected OMP controller without changing native defaults.
    Controller {
        /// Provider-free host protocol mode; never starts a model turn by itself.
        #[arg(long)]
        rpc: bool,
    },
    /// Manage explicitly selected, owner-installed external plugins.
    Plugins {
        /// Existing canonical private directory outside every checkout.
        #[arg(long)]
        registry: PathBuf,
        #[command(subcommand)]
        command: PluginCommands,
    },
    /// Verify, normalize and durably admit one event through an explicitly approved automation.
    Connector {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        automation: PathBuf,
        /// Bounded JSON envelope, or '-' for stdin. Authorization is never persisted.
        #[arg(long)]
        request: PathBuf,
    },
    /// Inspect this checkout and explicitly connect its native harness context.
    Init {
        #[arg(long)]
        harness: Option<HarnessId>,
        /// Read-only, noninteractive discovery.
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage explicit trusted-local native-context consent.
    Harness {
        #[command(subcommand)]
        command: onboarding::HarnessCommand,
    },
    /// Report platform support and discover local tools without executing them.
    Doctor {
        /// Emit a stable machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Discover registered adapters and nonbilling version-specific capability evidence.
    Harnesses {
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: HarnessesCommands,
    },
    /// Register or inspect a local Git source.
    Project {
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Execute one harness worker in a private prepared-tree fork.
    Run(RunArgs),
    /// Execute a task manifest through a bounded foreground worker pool.
    RunMany(RunManyArgs),
    /// Execute one versioned foreground API request.
    Api {
        #[arg(long)]
        request: PathBuf,
        #[arg(long, required = true)]
        json: bool,
        /// Deterministic fixture executable; not a production harness override.
        #[arg(long)]
        fake_binary: Option<PathBuf>,
    },
    /// Durably accept a version-1 task batch without waiting for its workers.
    Submit {
        #[arg(long)]
        request: PathBuf,
        #[arg(long, required = true)]
        json: bool,
        #[arg(long)]
        fake_binary: Option<PathBuf>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Spawn and reconnect to harness-independent durable workers.
    Workers {
        #[command(subcommand)]
        command: WorkersCommands,
    },
    /// Reconnect to durable batch or run lifecycle events.
    Status {
        id: String,
        #[arg(long, required = true)]
        json: bool,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Inspect independently captured results and durable artifact metadata.
    Results {
        id: String,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Cancel queued tasks or workers owned by the live local manager.
    Cancel {
        id: String,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Wait for completion, bounded by the supplied timeout.
    Wait {
        id: String,
        #[arg(long)]
        timeout_seconds: u64,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Operate the single trusted-local durable worker manager.
    Manager {
        #[command(subcommand)]
        command: ManagerCommands,
    },
    /// Inspect durable run and artifact metadata, including reclaimed workspaces.
    Runs {
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: RunsCommands,
    },
    /// Inspect a retained run; reclaim only with explicit --apply and verified evidence.
    Recover {
        id: fabric_domain::RunId,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect storage or reclaim fully captured private workspaces.
    Storage {
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: StorageCommands,
    },
}

#[derive(Subcommand)]
enum PluginCommands {
    List,
    Install {
        manifest: PathBuf,
    },
    Upgrade {
        manifest: PathBuf,
    },
    Revoke {
        id: HarnessId,
    },
    /// Perform a nonbilling startup, health and typed planning/evidence exchange.
    Check {
        id: HarnessId,
    },
}

fn plugins(root: PathBuf, command: PluginCommands) -> Result<(), FabricError> {
    use fabric_runner::plugin::{PluginHost, PluginKind, PluginManifest, PluginRegistry};
    let mut registry = PluginRegistry::load(&root)?;
    let upgrade = matches!(&command, PluginCommands::Upgrade { .. });
    match command {
        PluginCommands::List => return write_inspection(&registry.reports(), true),
        PluginCommands::Install { manifest } | PluginCommands::Upgrade { manifest } => {
            let manifest = PluginManifest::load(&manifest)?;
            registry.install(manifest, upgrade)?;
        }
        PluginCommands::Revoke { id } => registry.revoke(&id)?,
        PluginCommands::Check { id } => {
            let kind = registry
                .plugins
                .iter()
                .find(|p| p.manifest.id == id)
                .ok_or_else(plugin_input_error)?
                .manifest
                .kind;
            let manifest = registry.select(&id, kind)?;
            let token = fabric_runner::ForegroundInterrupt::install()?;
            let host = PluginHost::default();
            match kind {
                PluginKind::Harness => {
                    host.harness_info(&manifest, &token.token())?;
                }
                PluginKind::Target => {
                    let pin =
                        fabric_runner::target::TargetPin::from_manifest(manifest, id.as_str())?;
                    pin.open(&token.token())?.finish()?;
                }
                PluginKind::Connector => {
                    host.connector_capabilities(&manifest, &token.token())?;
                }
            }
            return write_inspection(&serde_json::json!({"compatible":true,"healthy":true}), true);
        }
    }
    registry.save(&root)?;
    write_inspection(&registry.reports(), true)
}

fn plugin_input_error() -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidArguments,
        message: "Invalid plugin input.".into(),
    }
}

#[derive(Subcommand)]
enum WorkersCommands {
    Spawn(SpawnArgs),
    Show {
        id: String,
        #[arg(long, required = true)]
        json: bool,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    Results {
        id: String,
        #[arg(long, required = true)]
        json: bool,
    },
    Cancel {
        id: String,
        #[arg(long, required = true)]
        json: bool,
    },
    Wait {
        id: String,
        #[arg(long)]
        timeout_seconds: u64,
        #[arg(long, required = true)]
        json: bool,
    },
}

#[derive(Args)]
struct SpawnArgs {
    #[arg(long, required_unless_present = "project", conflicts_with_all = ["project", "harness", "prompt", "task_id", "model", "prepared_state", "commit"])]
    request: Option<PathBuf>,
    #[arg(long, required_unless_present = "request")]
    project: Option<String>,
    #[arg(long, required_unless_present = "request")]
    harness: Option<HarnessId>,
    #[arg(long, required_unless_present = "request")]
    prompt: Option<String>,
    #[arg(long)]
    task_id: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, conflicts_with = "commit")]
    prepared_state: Option<Box<str>>,
    #[arg(long)]
    commit: Option<Box<str>>,
    #[arg(long, required = true)]
    json: bool,
    #[arg(long)]
    fake_binary: Option<PathBuf>,
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Subcommand)]
enum ManagerCommands {
    Serve {
        #[arg(long, required = true)]
        json: bool,
    },
    Stop {
        #[arg(long, required = true)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    name: String,
    #[arg(long)]
    prompt_file: PathBuf,
    #[arg(long)]
    harness: HarnessId,
    #[command(flatten)]
    options: WorkerArgs,
}

#[derive(clap::Args)]
struct RunManyArgs {
    name: String,
    /// JSON list of {id, prompt_file, harness?, request?}; prompts resolve against its parent.
    #[arg(long)]
    tasks: PathBuf,
    #[arg(long)]
    concurrency: std::num::NonZeroUsize,
    /// Default harness, optional when every task selects its own.
    #[arg(long)]
    harness: Option<HarnessId>,
    #[command(flatten)]
    options: WorkerArgs,
}

#[derive(clap::Args)]
struct WorkerArgs {
    #[command(flatten)]
    storage: StorageArgs,
    /// Absolute fake executable; defaults to fabric-fake-harness beside fabric.
    #[arg(long)]
    fake_binary: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    /// Nonsecret profile manifest; native configuration and credentials require explicit references.
    #[arg(long)]
    profile: Option<PathBuf>,
    #[arg(long = "require-capability")]
    required_capabilities: Vec<Capability>,
    #[arg(long, default_value = "local")]
    placement: String,
    #[arg(long, default_value = "edit")]
    access: String,
    #[arg(long)]
    hard_read_only: bool,
    #[arg(long)]
    resume: Option<String>,
    /// Explicit whole-tree allowance for ordinary-copy fallback (not a quota).
    #[arg(long, default_value_t = 0)]
    copy_limit_bytes: u64,
    /// Combined stdout/stderr/events limit, including the reserved truncation marker.
    #[arg(long, default_value_t = fabric_domain::StoragePolicy::default().max_log_bytes_per_run,
        value_parser = clap::value_parser!(u64).range(fabric_runner::capture::TRUNCATION_MARKER.len() as u64..))]
    max_log_bytes: u64,
    /// Total durable artifacts, including prompt, patch, copied logs and manifest.
    #[arg(long, default_value_t = fabric_domain::StoragePolicy::default().max_artifact_bytes_per_run,
        value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    max_artifact_bytes: u64,
    /// Per-worker wall-clock limit, enforced by Fabric (not a spending quota).
    #[arg(long, default_value = "3600")]
    timeout_seconds: std::num::NonZeroU64,
    /// Adapter-specific best-effort request budget; not a spending quota.
    #[arg(long)]
    max_cost_usd: Option<f64>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct StorageArgs {
    /// Best-effort logical managed-root cap, not an APFS hard quota.
    #[arg(long, default_value_t = fabric_domain::StoragePolicy::default().max_managed_bytes)]
    max_managed_bytes: u64,
    #[arg(long, default_value_t = fabric_domain::StoragePolicy::default().min_free_bytes)]
    min_free_bytes: u64,
    /// Periodically enforced workspace tree limit.
    #[arg(long, default_value_t = fabric_domain::StoragePolicy::default().max_workspace_bytes)]
    max_workspace_bytes: u64,
}

impl StorageArgs {
    fn options(&self) -> fabric_core::workspace::storage::StorageOptions {
        fabric_core::workspace::storage::StorageOptions {
            policy: fabric_domain::StoragePolicy {
                max_managed_bytes: self.max_managed_bytes,
                min_free_bytes: self.min_free_bytes,
                max_workspace_bytes: self.max_workspace_bytes,
                ..fabric_domain::StoragePolicy::default()
            },
            ..fabric_core::workspace::storage::StorageOptions::default()
        }
    }
}

#[derive(Subcommand)]
enum HarnessesCommands {
    /// List registered harness IDs without invoking executables.
    List,
    /// Probe an installed harness without starting a worker or billing models.
    Doctor {
        id: HarnessId,
        #[arg(long)]
        fake_binary: Option<PathBuf>,
    },
}

#[derive(Serialize)]
struct HarnessDoctorReport {
    harness_id: HarnessId,
    version: String,
    manifest: HarnessCapabilityManifest,
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// Pin the current committed HEAD without importing local edits.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: PathBuf,
    },
    /// Report the persisted source identity.
    Status { name: String },
    /// Prepare current committed HEAD in a private managed snapshot.
    Prepare {
        name: String,
        #[arg(long)]
        recipe: PathBuf,
        #[command(flatten)]
        storage: StorageArgs,
    },
}

#[derive(Subcommand)]
enum RunsCommands {
    /// List persisted runs in stable Run ID order.
    List,
    /// Show run, workspace, process, retention and preparation provenance.
    Show { id: fabric_domain::RunId },
    /// List registered artifact paths, lengths and SHA-256 checksums.
    Artifacts { id: fabric_domain::RunId },
    /// Read content-free activity; raw native content requires explicit --raw.
    Logs {
        id: fabric_domain::RunId,
        /// Per-run observation revision, or byte offset for the selected raw stream.
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 16384, value_parser = clap::value_parser!(u64).range(1..=65536))]
        limit_bytes: u64,
        /// Ctrl-C stops this viewer only; use `fabric cancel` to stop work.
        #[arg(long)]
        follow: bool,
        /// SENSITIVE local opt-in: no universal redaction; never upload these logs.
        #[arg(long, value_parser = ["stdout", "stderr", "events"])]
        raw: Option<String>,
    },
}

#[derive(Subcommand)]
enum StorageCommands {
    /// Best-effort logical usage, headroom, reservations and retention reasons.
    Status {
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Verify all deletion prerequisites; apply only with an explicit flag.
    Gc {
        #[arg(long, required_unless_present = "apply", conflicts_with = "apply")]
        dry_run: bool,
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Serialize)]
struct ProjectReport<'a> {
    project_id: &'a str,
    name: &'a str,
    source_path: &'a str,
    default_ref: &'a str,
    source_commit_sha: &'a str,
    created_at: i64,
}

#[derive(Serialize)]
struct PreparationReport<'a> {
    state_id: &'a str,
    project_id: &'a str,
    preparation_key: &'a str,
    source_commit_sha: &'a str,
    recipe_digest: &'a str,
    environment_digest: Option<&'a str>,
    prepared_path: &'a str,
    status: fabric_domain::PreparedStateStatus,
    created_at: i64,
    verified_at: Option<i64>,
    size_bytes: u64,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    error: &'a FabricError,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if error.use_stderr() && std::env::args_os().any(|arg| arg == "--json") {
                report_error(
                    &FabricError {
                        kind: ErrorKind::InvalidArguments,
                        message: error.to_string(),
                    },
                    true,
                );
            } else if let Err(error) = error.print() {
                report_error(&output_error(error), false);
                return ExitCode::FAILURE;
            }
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    let (json, result) = match cli.command {
        Commands::Up(args) => (args.json, up::up(cli.root, args)),
        Commands::Connect(options) => (true, connect::run(cli.root, options)),
        Commands::ConnectAdmit {
            automation,
            fake_binary,
        } => (true, connect::admit(cli.root, automation, fake_binary)),
        Commands::Controller { rpc } => (false, up::controller(cli.root, rpc)),
        Commands::Init {
            harness,
            check,
            json,
        } => (json, onboarding::init(cli.root, harness, check, json)),
        Commands::Harness { command } => {
            let json = command.json();
            (json, onboarding::harness(cli.root, command))
        }
        Commands::Doctor { json } => (json, write_doctor(json)),
        Commands::Plugins { registry, command } => (true, plugins(registry, command)),
        Commands::Connector {
            registry,
            automation,
            request,
        } => (
            true,
            write_connector(cli.root, registry, automation, request),
        ),
        Commands::Harnesses { command, json } => (json, write_harnesses(command, json)),
        Commands::Project { command, json } => (json, write_project(cli.root, command, json)),
        Commands::Run(args) => (args.options.json, write_run(cli.root, args)),
        Commands::RunMany(args) => (args.options.json, write_run_many(cli.root, args)),
        Commands::Api {
            request,
            json,
            fake_binary,
        } => {
            return match write_api(cli.root, request, fake_binary) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(error) => {
                    report_error(&error, json);
                    ExitCode::FAILURE
                }
            };
        }
        Commands::Submit {
            request,
            json,
            fake_binary,
            idempotency_key,
        } => (
            json,
            write_submit(cli.root, request, fake_binary, idempotency_key),
        ),
        Commands::Workers { command } => match command {
            WorkersCommands::Spawn(args) => (args.json, write_spawn(cli.root, args)),
            WorkersCommands::Show { id, json, after } => (
                json,
                write_worker(
                    cli.root,
                    fabric_core::manager::Request::Status { id, after },
                ),
            ),
            WorkersCommands::Results { id, json } => (
                json,
                write_worker(cli.root, fabric_core::manager::Request::Results { id }),
            ),
            WorkersCommands::Cancel { id, json } => (
                json,
                write_worker(cli.root, fabric_core::manager::Request::Cancel { id }),
            ),
            WorkersCommands::Wait {
                id,
                json,
                timeout_seconds,
            } => (
                json,
                write_worker(
                    cli.root,
                    fabric_core::manager::Request::Wait {
                        id,
                        timeout_seconds,
                    },
                ),
            ),
        },
        Commands::Status { id, json, after } => (
            json,
            write_worker(
                cli.root,
                fabric_core::manager::Request::Status { id, after },
            ),
        ),
        Commands::Results { id, json } => (
            json,
            write_worker(cli.root, fabric_core::manager::Request::Results { id }),
        ),
        Commands::Cancel { id, json } => (
            json,
            write_worker(cli.root, fabric_core::manager::Request::Cancel { id }),
        ),
        Commands::Wait {
            id,
            json,
            timeout_seconds,
        } => (
            json,
            write_worker(
                cli.root,
                fabric_core::manager::Request::Wait {
                    id,
                    timeout_seconds,
                },
            ),
        ),
        Commands::Manager {
            command: ManagerCommands::Stop { json },
        } => (
            json,
            write_worker(cli.root, fabric_core::manager::Request::Stop),
        ),
        Commands::Manager {
            command: ManagerCommands::Serve { json },
        } => (
            json,
            (|| {
                let root = cli.root.map_or_else(fabric_project::default_root, Ok)?;
                fabric_core::manager::serve(&root, |ready| write_inspection(&ready, true))
            })(),
        ),
        Commands::Runs { command, json } => (json, write_runs(cli.root, command, json)),
        Commands::Recover { id, apply, json } => (json, write_recover(cli.root, id, apply, json)),
        Commands::Storage { command, json } => (json, write_storage(cli.root, command, json)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error, json);
            ExitCode::FAILURE
        }
    }
}

fn write_worker(
    root: Option<PathBuf>,
    request: fabric_core::manager::Request,
) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let report = fabric_core::manager::request(&root, request)?;
    write_inspection(&report, true)
}

fn write_connector(
    root: Option<PathBuf>,
    registry: PathBuf,
    automation: PathBuf,
    path: PathBuf,
) -> Result<(), FabricError> {
    use std::io::Read;
    let automation = fabric_core::connector::read_automation(&automation)?;
    let event = if path == std::path::Path::new("-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(output_error)?;
        if bytes.len() > 65536 {
            return Err(plugin_input_error());
        }
        serde_json::from_slice(&bytes).map_err(|_| plugin_input_error())?
    } else {
        fabric_core::connector::read_incoming(&path)?
    };
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let report = fabric_core::connector::receive(&root, &registry, automation, event)?;
    write_inspection(&report, true)
}
fn write_submit(
    root: Option<PathBuf>,
    path: PathBuf,
    fake_binary: Option<PathBuf>,
    idempotency_key: Option<String>,
) -> Result<(), FabricError> {
    let file = std::fs::File::open(path).map_err(|_| FabricError {
        kind: ErrorKind::InvalidArguments,
        message: "Cannot open API request JSON file.".into(),
    })?;
    if !file.metadata().map_err(output_error)?.is_file() {
        return Err(FabricError {
            kind: ErrorKind::InvalidArguments,
            message: "API request must be a regular file.".into(),
        });
    }
    let request = fabric_core::api::read_request(file)?;
    write_worker(
        root,
        fabric_core::manager::Request::Submit {
            request,
            fake_binary,
            idempotency_key,
        },
    )
}

fn write_spawn(root: Option<PathBuf>, args: SpawnArgs) -> Result<(), FabricError> {
    let request = if let Some(path) = args.request {
        let file = std::fs::File::open(path).map_err(|_| FabricError {
            kind: ErrorKind::InvalidArguments,
            message: "Cannot open worker request JSON file.".into(),
        })?;
        if !file.metadata().map_err(output_error)?.is_file() {
            return Err(FabricError {
                kind: ErrorKind::InvalidArguments,
                message: "Worker request must be a regular file.".into(),
            });
        }
        fabric_core::api::read_worker_request(file)?
    } else {
        let missing = || FabricError {
            kind: ErrorKind::InvalidArguments,
            message: "Inline workers require --project, --harness, and --prompt.".into(),
        };
        let source_ref = args
            .prepared_state
            .map(|state_id| SourceRef::Prepared {
                state_id: state_id.into(),
            })
            .or_else(|| args.commit.map(|sha| SourceRef::Commit { sha: sha.into() }));
        fabric_core::api::WorkerBatchRequest {
            project: args.project.ok_or_else(missing)?,
            max_concurrency: 1,
            placement: "local".into(),
            tasks: vec![WorkerTask {
                task_id: args.task_id.unwrap_or_else(|| "task".into()),
                harness_id: args.harness.ok_or_else(missing)?,
                prompt: args.prompt.ok_or_else(missing)?,
                execution_context: None,
                source_ref,
                model: args.model,
                limits: None,
            }],
        }
    };
    write_worker(
        root,
        fabric_core::manager::Request::Spawn {
            request,
            fake_binary: args.fake_binary,
            idempotency_key: args.idempotency_key,
        },
    )
}

fn write_api(
    root: Option<PathBuf>,
    request: PathBuf,
    fake_binary: Option<PathBuf>,
) -> Result<bool, FabricError> {
    let invalid = || FabricError {
        kind: ErrorKind::InvalidArguments,
        message: "Cannot open API request JSON file.".into(),
    };
    let file = std::fs::File::open(request).map_err(|_| invalid())?;
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    let request = fabric_core::api::read_request(file)?;
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let interrupt = fabric_runner::ForegroundInterrupt::install()?;
    let response = fabric_core::api::spawn_many(
        &root,
        request,
        &fabric_runner::HarnessRegistry::default(),
        &fabric_runner::RegistryOptions { fake_binary },
        interrupt.token(),
    )?;
    write_inspection(&response, true)?;
    Ok(response.batch.succeeded)
}

fn write_run(root: Option<PathBuf>, args: RunArgs) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let interrupt = fabric_runner::ForegroundInterrupt::install()?;
    let options = run_options(&args.options, interrupt.token())?;
    let id = &args.harness;
    let harness = fabric_runner::HarnessRegistry::default().resolve(
        id,
        &fabric_runner::RegistryOptions {
            fake_binary: args.options.fake_binary.clone(),
        },
    )?;
    let report = fabric_core::run::foreground(
        &root,
        &args.name,
        &args.prompt_file,
        harness.as_ref(),
        &options,
    )?;
    let mut stdout = io::stdout().lock();
    if args.options.json {
        serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
        writeln!(stdout).map_err(output_error)?;
    } else {
        writeln!(
            stdout,
            "Run: {}\nState: {:?}\nExit: {:?}\nWorkspace: {}",
            report.run_id, report.state, report.exit_code, report.workspace_path
        )
        .map_err(output_error)?;
        if let Some(cleanup) = &report.cleanup {
            writeln!(stdout, "Cleanup: {} ({})", cleanup.action, cleanup.reason)
                .map_err(output_error)?;
            if let Some(detail) = &cleanup.detail {
                writeln!(stdout, "  {detail}").map_err(output_error)?;
            }
        }
    }
    stdout.flush().map_err(output_error)?;
    if report.state != fabric_domain::RunState::Succeeded {
        return Err(output_error(format!(
            "Run {} failed (exit {:?}); recovery artifacts: {:?}.",
            report.run_id, report.exit_code, report.artifact_manifest_id
        )));
    }
    Ok(())
}

fn run_options(
    args: &WorkerArgs,
    cancellation: fabric_runner::CancellationToken,
) -> Result<fabric_core::run::RunOptions, FabricError> {
    Ok(fabric_core::run::RunOptions {
        copy_limit_bytes: args.copy_limit_bytes,
        max_log_bytes: args.max_log_bytes,
        max_artifact_bytes: args.max_artifact_bytes,
        limits: fabric_runner::ExecutionLimits {
            timeout: std::time::Duration::from_secs(args.timeout_seconds.get()),
            ..fabric_runner::ExecutionLimits::default()
        },
        cancellation,
        target_pin: None,
        max_cost_usd: args.max_cost_usd,
        storage: args.storage.options(),
        harness_request: HarnessRequest {
            model: args.model.clone(),
            required_capabilities: args.required_capabilities.clone(),
            placement: args.placement.clone(),
            access: args.access.clone(),
            hard_read_only: args.hard_read_only,
            resume: args.resume.clone(),
            max_cost_usd: None,
            profile: args.profile.as_deref().map(read_profile).transpose()?,
        },
    })
}

fn read_profile(path: &std::path::Path) -> Result<AgentProfile, FabricError> {
    const MAX_PROFILE_BYTES: u64 = 64 * 1024;
    let invalid = || FabricError {
        kind: ErrorKind::InvalidArguments,
        message:
            "Cannot load profile manifest: use valid nonsecret profile JSON of at most 64 KiB."
                .into(),
    };
    let file = std::fs::File::open(path).map_err(|_| invalid())?;
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
fn write_run_many(root: Option<PathBuf>, args: RunManyArgs) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let interrupt = fabric_runner::ForegroundInterrupt::install()?;
    let options = run_options(&args.options, interrupt.token())?;
    let registry = fabric_runner::HarnessRegistry::default();
    let registry_options = fabric_runner::RegistryOptions {
        fake_binary: args.options.fake_binary.clone(),
    };
    let report = fabric_core::run::run_many(
        &root,
        &args.name,
        &args.tasks,
        args.concurrency.get(),
        &fabric_core::run::BatchHarnesses {
            registry: &registry,
            options: &registry_options,
            default: args.harness.as_ref(),
        },
        &options,
    )?;
    let mut stdout = io::stdout().lock();
    if args.options.json {
        serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
        writeln!(stdout).map_err(output_error)?;
    } else {
        writeln!(
            stdout,
            "Batch: {}\nSucceeded: {}",
            report.batch_id, report.succeeded
        )
        .map_err(output_error)?;
        for task in &report.tasks {
            writeln!(
                stdout,
                "Task {}: Run {} — {}",
                task.id,
                task.run_id,
                match &task.result {
                    Some(run) => format!("{:?} (exit {:?})", run.state, run.exit_code),
                    None => task.error.as_deref().unwrap_or("incomplete").to_owned(),
                }
            )
            .map_err(output_error)?;
        }
    }
    stdout.flush().map_err(output_error)?;
    if !report.succeeded {
        return Err(output_error(format!(
            "Batch {} failed; individual results are preserved.",
            report.batch_id
        )));
    }
    Ok(())
}

fn write_project(
    root: Option<PathBuf>,
    command: ProjectCommands,
    json: bool,
) -> Result<(), FabricError> {
    let root = match root {
        Some(root) => root,
        None => fabric_project::default_root()?,
    };
    if let ProjectCommands::Prepare {
        name,
        recipe,
        storage,
    } = command
    {
        let state =
            fabric_core::workspace::storage::prepare(&root, &name, &recipe, &storage.options())?;
        let report = PreparationReport {
            state_id: state.state_id.as_str(),
            project_id: state.project_id.as_str(),
            preparation_key: &state.preparation_key,
            source_commit_sha: &state.source_commit_sha,
            recipe_digest: &state.recipe_digest,
            environment_digest: state.harness_environment_digest.as_deref(),
            prepared_path: &state.prepared_path,
            status: state.status,
            created_at: state.created_at,
            verified_at: state.verified_at,
            size_bytes: state.size_bytes,
        };
        let mut stdout = io::stdout().lock();
        if json {
            serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
            writeln!(stdout).map_err(output_error)?;
        } else {
            writeln!(
                stdout,
                "Prepared: {}\nStatus: READY\nSource SHA: {}\nPath: {}\nSize: {} bytes",
                report.state_id, report.source_commit_sha, report.prepared_path, report.size_bytes
            )
            .map_err(output_error)?;
        }
        return stdout.flush().map_err(output_error);
    }
    let project = match command {
        ProjectCommands::Add { name, path } => {
            let registration = fabric_project::register(&root, &name, &path)?;
            if registration.dirty {
                writeln!(
                    io::stderr().lock(),
                    "warning: dirty Git checkout; only committed HEAD is pinned; uncommitted changes are not imported."
                )
                .map_err(output_error)?;
            }
            registration.project
        }
        ProjectCommands::Status { name } => {
            return write_inspection(&fabric_core::inspect::project(&root, &name)?, json);
        }
        ProjectCommands::Prepare { .. } => unreachable!("preparation handled above"),
    };
    let report = ProjectReport {
        project_id: project.project_id.as_str(),
        name: &project.name,
        source_path: &project.source_path,
        default_ref: &project.default_ref,
        source_commit_sha: &project.default_ref,
        created_at: project.created_at,
    };
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
        writeln!(stdout).map_err(output_error)?;
    } else {
        writeln!(
            stdout,
            "Project: {}\nSource: {}\nSource SHA: {}",
            report.name, report.source_path, report.source_commit_sha
        )
        .map_err(output_error)?;
    }
    stdout.flush().map_err(output_error)
}

fn write_runs(root: Option<PathBuf>, command: RunsCommands, json: bool) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    match command {
        RunsCommands::List => write_inspection(&fabric_core::inspect::list(&root)?, json),
        RunsCommands::Show { id } => {
            write_inspection(&fabric_core::inspect::show(&root, &id)?, json)
        }
        RunsCommands::Artifacts { id } => {
            write_inspection(&fabric_core::inspect::artifacts(&root, &id)?, json)
        }
        RunsCommands::Logs {
            id,
            after,
            limit_bytes,
            follow,
            raw,
        } => {
            let stream = raw.as_deref().map(|stream| match stream {
                "stdout" => fabric_core::logs::RawStream::Stdout,
                "stderr" => fabric_core::logs::RawStream::Stderr,
                _ => fabric_core::logs::RawStream::Events,
            });
            if stream.is_some() {
                eprintln!(
                    "SENSITIVE LOCAL RAW LOGS: may contain prompts, credentials, reasoning, tool arguments/results and native errors. No universal secret redaction. Do not post/upload to GitHub or Connect."
                );
            }
            let interrupt = fabric_runner::ForegroundInterrupt::install()?;
            let cancellation = interrupt.token();
            let mut cursor = after;
            loop {
                if cancellation.is_cancelled() {
                    return Ok(());
                }
                let report =
                    fabric_core::logs::read(&root, &id, cursor, limit_bytes as usize, stream)?;
                if !follow || report.changed {
                    write_inspection(&report, json || follow)?;
                }
                cursor = report.next_cursor;
                if !follow || (report.terminal && (stream.is_none() || report.raw_eof)) {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
}

fn write_inspection(report: &impl Serialize, json: bool) -> Result<(), FabricError> {
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, report).map_err(output_error)?;
    } else {
        serde_json::to_writer_pretty(&mut stdout, report).map_err(output_error)?;
    }
    writeln!(stdout).map_err(output_error)?;
    stdout.flush().map_err(output_error)
}

fn write_recover(
    root: Option<PathBuf>,
    id: fabric_domain::RunId,
    apply: bool,
    json: bool,
) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let report = fabric_core::workspace::recover(&root, &id, apply)?;
    if json {
        return write_inspection(&report, true);
    }
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "Recovery ({}): run {} / workspace {}\n{} ({})",
        report.mode, report.run_id, report.workspace_id, report.action, report.reason
    )
    .map_err(output_error)?;
    if let Some(detail) = &report.detail {
        writeln!(stdout, "{detail}").map_err(output_error)?;
    }
    if report.action == "would_recover" {
        writeln!(
            stdout,
            "Apply verified recovery: fabric recover {} --apply",
            report.run_id
        )
        .map_err(output_error)?;
    } else if report.action == "retained" {
        writeln!(stdout, "Workspace retained for safety; resolve the reason above, then inspect again with fabric recover {} --json.", report.run_id)
            .map_err(output_error)?;
    }
    stdout.flush().map_err(output_error)
}

fn write_storage(
    root: Option<PathBuf>,
    command: StorageCommands,
    json: bool,
) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    if let StorageCommands::Status { storage } = command {
        let report = fabric_core::workspace::storage::status(&root, &storage.options())?;
        let mut stdout = io::stdout().lock();
        if json {
            serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
            writeln!(stdout).map_err(output_error)?;
        } else {
            writeln!(stdout, "Storage: {} logical bytes (best effort; not exclusive APFS bytes)\nAvailable: {} bytes\nReserved: {} bytes",
                report.managed_bytes, report.free_bytes, report.reserved_bytes).map_err(output_error)?;
            for item in report.caches.iter().chain(&report.workspaces) {
                writeln!(
                    stdout,
                    "{}{}: {} logical bytes ({})\n  {}{}",
                    item.id,
                    item.run_id
                        .as_ref()
                        .map(|id| format!(" / run {id}"))
                        .unwrap_or_default(),
                    item.usage_bytes
                        .map(|bytes| bytes.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    item.retention_reason,
                    item.action,
                    item.diagnostic
                        .as_ref()
                        .map(|diagnostic| format!("\n  {diagnostic}"))
                        .unwrap_or_default()
                )
                .map_err(output_error)?;
            }
        }
        return stdout.flush().map_err(output_error);
    }
    let StorageCommands::Gc { apply, .. } = command else {
        unreachable!()
    };
    let report = fabric_core::workspace::garbage_collect(&root, apply)?;
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
        writeln!(stdout).map_err(output_error)?;
    } else {
        writeln!(stdout, "Storage GC ({})", report.mode).map_err(output_error)?;
        for item in &report.items {
            writeln!(
                stdout,
                "{}: {} ({}){}",
                item.workspace_id,
                item.action,
                item.reason,
                item.detail
                    .as_ref()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            )
            .map_err(output_error)?;
        }
        for item in &report.unknown_paths {
            writeln!(stdout, "{}: refused ({})", item.path, item.reason).map_err(output_error)?;
        }
    }
    stdout.flush().map_err(output_error)
}

fn write_harnesses(command: HarnessesCommands, json: bool) -> Result<(), FabricError> {
    let registry = fabric_runner::HarnessRegistry::default();
    match command {
        HarnessesCommands::List => write_inspection(&registry.ids()?, json),
        HarnessesCommands::Doctor { id, fake_binary } => {
            let harness = registry.resolve(&id, &fabric_runner::RegistryOptions { fake_binary })?;
            let info = harness.probe()?;
            let manifest = harness.capabilities(&info)?;
            write_inspection(
                &HarnessDoctorReport {
                    harness_id: id,
                    version: info.version,
                    manifest,
                },
                json,
            )
        }
    }
}

fn write_doctor(json: bool) -> Result<(), FabricError> {
    let report = fabric_core::doctor()?;
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, &report).map_err(output_error)?;
        writeln!(stdout).map_err(output_error)?;
    } else {
        writeln!(
            stdout,
            "Platform: {} ({}) — {} (primary supported OS: macOS)",
            report.platform.os,
            report.platform.arch,
            if report.platform.supported {
                "supported"
            } else {
                "unsupported"
            },
        )
        .map_err(output_error)?;
        for tool in report.tools {
            match tool.path {
                Some(path) => writeln!(stdout, "{}: {}", tool.name, path.display()),
                None => writeln!(stdout, "{}: not found on PATH", tool.name),
            }
            .map_err(output_error)?;
        }
    }
    stdout.flush().map_err(output_error)
}

fn output_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Output,
        message: error.to_string(),
    }
}

fn report_error(error: &FabricError, json: bool) {
    let mut stderr = io::stderr().lock();
    if json {
        let _ = serde_json::to_writer(&mut stderr, &ErrorResponse { error });
        let _ = writeln!(stderr);
    } else {
        let _ = writeln!(stderr, "fabric: {error}");
    }
}
