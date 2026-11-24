//! Explicit target-side entry point; never launched by Core on the controller.
use std::io::Read;
use std::path::PathBuf;
fn main() {
    let result = (|| -> Result<(), fabric_domain::FabricError> {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() != 2 { return Err(fabric_runner::execution_error("Expected private HOME and work roots")); }
        let mut line = Vec::new();
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut byte = [0];
            if stdin.read(&mut byte).map_err(fabric_runner::execution_error)? != 1 || line.len() >= 16384 { return Err(fabric_runner::execution_error("Invalid remote binding")); }
            if byte[0] == b'\n' { break; } line.push(byte[0]);
        }
        drop(stdin);
        let binding: serde_json::Value = serde_json::from_slice(&line).map_err(fabric_runner::execution_error)?;
        let instance = std::env::var("FABRIC_TARGET_INSTANCE").map_err(fabric_runner::execution_error)?;
        fabric_core::remote_agent::serve(PathBuf::from(&args[0]),PathBuf::from(&args[1]),binding,instance)
    })();
    if result.is_err() {
        // Native errors and transport diagnostics can contain secrets. No raw stderr.
        eprintln!("Remote agent failed closed"); std::process::exit(1);
    }
}
