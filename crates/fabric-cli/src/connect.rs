use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use fabric_domain::{FabricError, NormalizedTrigger};

#[derive(clap::Args)]
pub struct Options {
    /// Private schema-2 scoped Connect credential file.
    #[arg(long)]
    connection: Option<PathBuf>,
    /// Private trusted local Automation policy with a pinned prepared source and explicit limits.
    #[arg(long)]
    automation: Option<PathBuf>,
    /// Explicit local fake harness executable; never selected by a webhook.
    #[arg(long, conflicts_with = "allow_model_spend")]
    fake_binary: Option<PathBuf>,
    /// Explicit authority to launch the locally configured non-fake harness.
    #[arg(long)]
    allow_model_spend: bool,
    /// Permit HTTP to a literal loopback address for provider-free checks only.
    #[arg(long)]
    allow_loopback_http: bool,
}

impl Options {
    pub fn selected(&self) -> bool {
        self.connection.is_some()
            || self.automation.is_some()
            || self.fake_binary.is_some()
            || self.allow_model_spend
            || self.allow_loopback_http
    }
}

pub fn run(root: Option<PathBuf>, options: Options) -> Result<(), FabricError> {
    let connection = options
        .connection
        .ok_or_else(|| super::onboarding::invalid("--connection is required for Connect."))?;
    let automation = options
        .automation
        .ok_or_else(|| super::onboarding::invalid("--automation is required for Connect."))?;
    if options.fake_binary.is_none() && !options.allow_model_spend {
        return Err(super::onboarding::invalid(
            "Connect requires --fake-binary or explicit --allow-model-spend.",
        ));
    }
    fabric_core::connector::read_automation(&automation)?;
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg(include_str!("../../../scripts/fabric-connect-agent.py"))
        .arg("--fabric")
        .arg(std::env::current_exe().map_err(super::output_error)?)
        .arg("--root")
        .arg(root)
        .arg("--connection")
        .arg(connection)
        .arg("--automation")
        .arg(automation);
    if let Some(fake) = options.fake_binary {
        command.arg("--fake-binary").arg(fake);
    }
    if options.allow_model_spend {
        command.arg("--allow-model-spend");
    }
    if options.allow_loopback_http {
        command.arg("--allow-loopback-http");
    }
    // Replace only the foreground connection process. Its manager and healthy tasks remain independent.
    Err(super::output_error(command.exec()))
}

pub fn admit(
    root: Option<PathBuf>,
    automation: PathBuf,
    fake_binary: Option<PathBuf>,
) -> Result<(), FabricError> {
    let automation = fabric_core::connector::read_automation(&automation)?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(super::output_error)?;
    if bytes.len() > 8192 {
        return Err(super::onboarding::invalid(
            "Connect event exceeds 8192 bytes.",
        ));
    }
    let trigger: NormalizedTrigger = serde_json::from_slice(&bytes)
        .map_err(|_| super::onboarding::invalid("Invalid normalized Connect event."))?;
    automation.connect_route(&trigger)?;
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let report = fabric_core::manager::request(
        &root,
        fabric_core::manager::Request::Connect {
            automation: Box::new(automation),
            trigger,
            fake_binary,
        },
    )?;
    super::write_inspection(&report, true)
}
