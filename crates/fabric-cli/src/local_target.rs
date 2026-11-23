//! Reference trusted-local driver. Core interprets these proposals with its own validated objects.
use std::io::{self, BufRead, Read, Write};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request { jsonrpc: String, id: u64, method: String, params: Value }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job { placement: String, run_id: String }

fn serve() -> Result<(), ()> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--fabric-plugin-stdio") || args.next().is_some() { return Err(()); }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut frame = Vec::with_capacity(1024);
    let mut id = 1u64;
    loop {
        frame.clear();
        input.by_ref().take(65_537).read_until(b'\n', &mut frame).map_err(|_| ())?;
        if frame.len() > 65_536 || frame.last() != Some(&b'\n') { return Err(()); }
        let request: Request = serde_json::from_slice(&frame).map_err(|_| ())?;
        if request.jsonrpc != "2.0" || request.id != id { return Err(()); }
        let shutdown = request.method == "shutdown";
        let result = if id == 1 {
            if request.method != "initialize" || request.params != json!({"protocol":1}) { return Err(()); }
            json!({"protocol":1,"id":"local","version":"0.1.0","kind":"target"})
        } else if id == 2 {
            if request.method != "health" || request.params != json!({}) { return Err(()); }
            json!({"ready":true})
        } else if shutdown {
            if request.params != json!({}) { return Err(()); }
            json!({"ready":true})
        } else if request.method == "target.capabilities" {
            if request.params != json!({}) { return Err(()); }
            json!({"placements":["local"],"operations":fabric_runner::target::OPERATIONS,"optional":[]})
        } else {
            let operation = request.method.strip_prefix("target.").ok_or(())?;
            if !fabric_runner::target::OPERATIONS.contains(&operation) { return Err(()); }
            let job: Job = serde_json::from_value(request.params).map_err(|_| ())?;
            if job.placement != "local" || job.run_id.is_empty() || job.run_id.len() > 128 { return Err(()); }
            json!({"operation":format!("local.{operation}")})
        };
        serde_json::to_writer(&mut output, &json!({"jsonrpc":"2.0","id":id,"result":result})).map_err(|_| ())?;
        output.write_all(b"\n").and_then(|()| output.flush()).map_err(|_| ())?;
        if shutdown { return Ok(()); }
        id = id.checked_add(1).ok_or(())?;
    }
}
fn main() -> std::process::ExitCode {
    if serve().is_ok() { std::process::ExitCode::SUCCESS } else {
        eprintln!("Fabric local target: invalid control exchange.");
        std::process::ExitCode::FAILURE
    }
}
