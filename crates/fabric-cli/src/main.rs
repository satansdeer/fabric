use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fabric_domain::{ErrorKind, FabricError};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "fabric", version, about = "Trusted-local agent workspaces")]
struct Cli {
    /// Private Fabric storage root (default: $XDG_DATA_HOME/fabric or ~/.local/share/fabric).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Report platform support and discover local tools without executing them.
    Doctor {
        /// Emit a stable machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Register or inspect a local Git source.
    Project {
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: ProjectCommands,
    },
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
        Commands::Doctor { json } => (json, write_doctor(json)),
        Commands::Project { command, json } => (json, write_project(cli.root, command, json)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error, json);
            ExitCode::FAILURE
        }
    }
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
    if let ProjectCommands::Prepare { name, recipe } = command {
        let state = fabric_project::prepare::prepare(&root, &name, &recipe)?;
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
        ProjectCommands::Status { name } => fabric_project::status(&root, &name)?,
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

fn write_doctor(json: bool) -> Result<(), FabricError> {
    let report = fabric_core::doctor();
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
