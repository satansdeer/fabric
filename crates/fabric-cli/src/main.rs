use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fabric_domain::{ErrorKind, FabricError};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "fabric", version, about = "Trusted-local agent workspaces")]
struct Cli {
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
    let Commands::Doctor { json } = cli.command;
    match write_doctor(json) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error, json);
            ExitCode::FAILURE
        }
    }
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
