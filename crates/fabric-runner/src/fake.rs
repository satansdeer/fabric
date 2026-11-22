//! Deterministic external fixture protocol, not a mocked process result.
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    #[serde(default)]
    required_files: Vec<String>,
    #[serde(default)]
    writes: Vec<WriteFile>,
    #[serde(default)]
    events: Vec<serde_json::Value>,
    #[serde(default)]
    absent_environment: Vec<String>,
    #[serde(default)]
    exit_code: u8,
    #[serde(default)]
    process_tree: Option<ProcessTree>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessTree {
    #[serde(default)]
    ignore_child_term: bool,
    #[serde(default)]
    leader_exits: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFile {
    path: String,
    content: String,
}

pub fn main() -> u8 {
    match run() {
        Ok(exit) => exit,
        Err(error) => {
            eprintln!("fake harness: {error}");
            2
        }
    }
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args == ["--version"] {
        println!("fabric-fake-harness 1");
        return Ok(0);
    }
    if args == ["--protocol-a", "--version"] {
        println!("fabric-fake-a 1");
        return Ok(0);
    }
    if args == ["--protocol-b", "--version"] {
        println!("fabric-fake-b 1");
        return Ok(0);
    }
    if args.len() == 2 && args[0] == "--heartbeat-child" {
        if args[1] == "ignore-term" {
            // SAFETY: deterministic fixture intentionally exercises KILL escalation.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        }
        heartbeat("child.heartbeat")?;
        unreachable!();
    }
    if args.len() == 5 && args[0] == "--protocol-a" && args[1] == "--input" && args[3] == "--model"
    {
        let model = args[4].to_str().ok_or("model must be UTF-8")?;
        let request = fabric_domain::HarnessRequest {
            model: Some(model.into()),
            ..Default::default()
        };
        request.validate()?;
        let task: Task = serde_json::from_slice(&fs::read(&args[2])?)?;
        return run_a(task, model);
    }
    if args.len() == 3 && args[0] == "--protocol-b" && args[1] == "--task" {
        let task: Task = serde_json::from_slice(&fs::read(&args[2])?)?;
        return run_b(task);
    }
    if args.len() != 2 || args[0] != "--prompt-file" {
        return Err("expected --prompt-file PATH".into());
    }
    let task: Task = serde_json::from_slice(&fs::read(&args[1])?)?;
    let mut stdout = io::stdout().lock();
    let mut event = |value: &serde_json::Value| -> io::Result<()> {
        serde_json::to_writer(&mut stdout, value)?;
        writeln!(stdout)?;
        stdout.flush()
    };
    event(&serde_json::json!({"type":"started"}))?;
    execute_mutations(&task)?;
    for value in &task.events {
        event(value)?;
    }
    execute_process_tree(&task)?;
    event(&serde_json::json!({"type":"finished", "exit_code":task.exit_code}))?;
    Ok(task.exit_code)
}

fn run_a(task: Task, model: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    let mut emit = |value: &serde_json::Value| -> io::Result<()> {
        serde_json::to_writer(&mut stdout, value)?;
        writeln!(stdout)?;
        stdout.flush()
    };
    emit(&serde_json::json!({"kind":"model","model":model}))?;
    emit(&serde_json::json!({"kind":"output","body":format!("fake-a model={model}")}))?;
    emit(&serde_json::json!({"kind":"tool","name":"fixture-write","phase":"start"}))?;
    execute_mutations(&task)?;
    emit(&serde_json::json!({"kind":"tool","name":"fixture-write","phase":"end"}))?;
    for value in &task.events {
        emit(value)?;
    }
    // Fixture token units, not provider billing or a fabricated dollar price.
    emit(&serde_json::json!({"kind":"usage","input":1,"output":1,"cached":0}))?;
    execute_process_tree(&task)?;
    Ok(task.exit_code)
}

fn run_b(task: Task) -> Result<u8, Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "OUTPUT fake-b plaintext")?;
    writeln!(stdout, "TOOL start fixture-write")?;
    stdout.flush()?;
    execute_mutations(&task)?;
    writeln!(stdout, "TOOL end fixture-write")?;
    for value in &task.events {
        if let Some(line) = value.as_str() {
            writeln!(stdout, "{line}")?;
        } else {
            return Err("fake-b task events must be plaintext strings".into());
        }
    }
    stdout.flush()?;
    execute_process_tree(&task)?;
    Ok(task.exit_code)
}

fn execute_mutations(task: &Task) -> Result<(), Box<dyn std::error::Error>> {
    for name in &task.absent_environment {
        if std::env::var_os(name).is_some() {
            return Err("unexpected inherited environment".into());
        }
    }
    for path in &task.required_files {
        validate_path(Path::new(path))?;
        fs::read(path)?;
    }
    for write in &task.writes {
        let path = Path::new(&write.path);
        validate_path(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &write.content)?;
    }
    Ok(())
}

fn execute_process_tree(task: &Task) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(tree) = &task.process_tree {
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--heartbeat-child",
                if tree.ignore_child_term {
                    "ignore-term"
                } else {
                    "term"
                },
            ])
            .spawn()?;
        let start = std::time::Instant::now();
        while !Path::new("child.heartbeat").exists() {
            if child.try_wait()?.is_some() || start.elapsed() > std::time::Duration::from_secs(5) {
                return Err("heartbeat child did not become ready".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if tree.leader_exits {
            fs::write("parent.heartbeat", b"leader exited\n")?;
        } else {
            heartbeat("parent.heartbeat")?;
        }
    }
    Ok(())
}

fn heartbeat(path: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    writeln!(file, "pid={}", std::process::id())?;
    loop {
        file.write_all(b".")?;
        file.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn validate_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::other(
            "fixture paths must be relative normal components",
        ));
    }
    let mut current = std::path::PathBuf::new();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::other("fixture path contains symlink"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
