use fabric_domain::{ErrorKind, HarnessRequest, ProfileEvidence};
use fabric_reference_harnesses::{
    AgentSpec, CodexHarness, HarnessAdapter, HarnessInfo, NativeEvent, OmpHarness,
};
use fabric_runner::native_plugin::{
    NativeArg, NativeCallContext, NativeCommand, NativePlan, NativeReply,
};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn call(id: &str, method: &str, params: Value) -> Result<Value, ErrorKind> {
    let context: NativeCallContext =
        serde_json::from_value(params.get("context").cloned().unwrap_or_else(|| json!({})))
            .map_err(|_| ErrorKind::InvalidArguments)?;
    // The host supplies path metadata and credential availability, never credentials.
    for (name, value) in context.environment {
        unsafe {
            std::env::set_var(name, value);
        }
    }
    for (name, available) in context.credentials {
        unsafe {
            if available {
                std::env::set_var(name, "fabric-credential-available");
            } else {
                std::env::remove_var(name);
            }
        }
    }
    let binary = context.binary.unwrap_or_else(|| "/usr/bin/false".into());
    let adapter: Box<dyn HarnessAdapter> = match id {
        "omp" => Box::new(OmpHarness::new(binary)),
        "codex" => Box::new(CodexHarness::new(binary)),
        _ => return Err(ErrorKind::UnknownHarness),
    };
    let decode = || {
        serde_json::from_value::<HarnessRequest>(params["request"].clone())
            .map_err(|_| ErrorKind::InvalidArguments)
    };
    let info = HarnessInfo {
        version: params["version"].as_str().unwrap_or("").to_owned(),
    };
    match method {
        "harness.probe" => adapter
            .probe()
            .map(|info| json!({"version":info.version}))
            .map_err(|e| e.kind),
        "harness.native_context" => adapter
            .native_context()
            .map(|v| json!(v))
            .map_err(|e| e.kind),
        "harness.capabilities" => adapter
            .capabilities(&info)
            .map(|v| json!(v))
            .map_err(|e| e.kind),
        "harness.plan" => adapter
            .plan(&decode()?, &info)
            .map(|plan| {
                json!(NativePlan {
                    plan,
                    supports_cost_limit: id == "omp"
                })
            })
            .map_err(|e| e.kind),
        "harness.command" => {
            let request = decode()?;
            let profile: ProfileEvidence = serde_json::from_value(params["profile"].clone())
                .map_err(|_| ErrorKind::InvalidProfile)?;
            let prompt_file = std::path::Path::new(
                params["prompt_file"]
                    .as_str()
                    .ok_or(ErrorKind::InvalidArguments)?,
            );
            let spec = AgentSpec {
                request: &request,
                profile: &profile,
                prompt_file,
                max_cost_usd: request.max_cost_usd,
            };
            let command = adapter.command(&spec).map_err(|e| e.kind)?;
            let mut args = command
                .args
                .into_iter()
                .map(|s| {
                    s.into_string()
                        .map(NativeArg::Literal)
                        .map_err(|_| ErrorKind::InvalidArguments)
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Both reference CLIs take the literal prompt after their final '--'.
            if !matches!(args.last(), Some(NativeArg::Literal(_))) {
                return Err(ErrorKind::InvalidArguments);
            }
            *args.last_mut().unwrap() = NativeArg::Prompt { prompt: true };
            let environment = command
                .environment
                .into_iter()
                .map(|(k, v)| {
                    Ok((
                        k.into_string().map_err(|_| ErrorKind::InvalidArguments)?,
                        v.into_string().map_err(|_| ErrorKind::InvalidArguments)?,
                    ))
                })
                .collect::<Result<Vec<_>, ErrorKind>>()?;
            Ok(json!(NativeCommand {
                program: command.program,
                args,
                environment
            }))
        }
        "harness.normalize" => {
            let event = if let Some(text) = params["text"].as_str() {
                NativeEvent::Text(text)
            } else {
                NativeEvent::Json(&params["event"])
            };
            Ok(json!({"event":adapter.normalize_event(event)}))
        }
        _ => Err(ErrorKind::InvalidArguments),
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2
        || !["omp", "codex"].contains(&args[0].as_str())
        || args[1] != "--fabric-plugin-stdio"
    {
        return Err("invalid plugin invocation".into());
    }
    let id = &args[0];
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut expected = 1u64;
    loop {
        let mut bytes = Vec::new();
        use std::io::Read;
        let n = input
            .by_ref()
            .take((2 * fabric_runner::plugin::MAX_FRAME + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        if n > 2 * fabric_runner::plugin::MAX_FRAME || bytes.last() != Some(&b'\n') {
            return Err("invalid frame".into());
        }
        let request: Value = serde_json::from_slice(&bytes)?;
        if request["jsonrpc"] != "2.0" || request["id"] != expected {
            return Err("invalid request".into());
        }
        let method = request["method"].as_str().ok_or("missing method")?;
        if expected == 1 && (method != "initialize" || request["params"] != json!({"protocol":1}))
            || expected == 2 && method != "health"
        {
            return Err("invalid handshake".into());
        }
        let result = match method {
            "initialize" if expected == 1 => {
                json!({"protocol":1,"id":id,"version":"0.1.0","kind":"harness"})
            }
            "health" if expected == 2 => json!({"ready":true}),
            "shutdown" if expected >= 3 => json!({"ready":true}),
            _ if expected >= 3 => match call(id, method, request["params"].clone()) {
                Ok(result) => json!(NativeReply {
                    result: Some(result),
                    failure: None
                }),
                Err(failure) => json!(NativeReply::<Value> {
                    result: None,
                    failure: Some(failure)
                }),
            },
            _ => return Err("invalid method".into()),
        };
        let response = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":expected,"result":result}))?;
        if response.len() + 1 > 2 * fabric_runner::plugin::MAX_FRAME {
            return Err("response too large".into());
        }
        output.write_all(&response)?;
        output.write_all(b"\n")?;
        output.flush()?;
        expected = expected.checked_add(1).ok_or("id overflow")?;
        if method == "shutdown" {
            break;
        }
    }
    Ok(())
}
