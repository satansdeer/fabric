//! Target-side agent. No target, provider, cluster, or harness-specific dispatch.
//! The installed target supplies two private roots and an immutable opaque binding.
use crate::remote::{self, error};
use fabric_domain::{FabricError, HarnessId, HarnessPlan};
use fabric_runner::{CancellationToken, HarnessAdapter};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write, BufRead, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const HOME_ROOTS: [&str; 6] = ["", ".config", ".cache", ".local", ".local/share", ".local/state"];

#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RootIdentity { device: u64, inode: u64, birth_seconds: u64, birth_nanos: u32 }

fn root_identity(path: &Path) -> Result<RootIdentity, FabricError> {
    let meta = fs::symlink_metadata(path).map_err(error)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(error("unsafe home root"));
    }
    // Birth time disambiguates inode reuse; filesystems without it refuse.
    let birth = meta.created().map_err(error)?.duration_since(std::time::UNIX_EPOCH).map_err(error)?;
    Ok(RootIdentity { device:meta.dev(), inode:meta.ino(), birth_seconds:birth.as_secs(), birth_nanos:birth.subsec_nanos() })
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HomeOwner { environment: Value, container: RootIdentity, roots: [RootIdentity; 6] }

fn home_owner(container: &Path, environment: &Value) -> Result<HomeOwner, FabricError> {
    let tree = container.join("tree");
    let [home, config, cache, local, data, state] = HOME_ROOTS.map(|path| {
        if path.is_empty() { root_identity(&tree) } else { root_identity(&tree.join(path)) }
    });
    Ok(HomeOwner { environment:environment.clone(), container:root_identity(container)?,
        roots:[home?, config?, cache?, local?, data?, state?] })
}
fn initialize_home(container: &Path, environment: &Value) -> Result<(), FabricError> {
    match fs::symlink_metadata(container) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(container).map_err(error)?;
            let tree = container.join("tree");
            directory(&tree)?;
            for path in HOME_ROOTS.iter().skip(1) { directory(&tree.join(path))?; }
            let record = home_owner(container,environment)?;
            let mut marker = remote::private_file(&container.join(".fabric-owner"))?;
            marker.write_all(&serde_json::to_vec(&record).map_err(error)?).and_then(|()|marker.sync_all()).map_err(error)?;
            File::open(container).and_then(|f|f.sync_all()).map_err(error)?;
        }
        Err(e) => return Err(error(e)),
        Ok(_) => return Err(error("persistent HOME already exists")),
    }
    Ok(())
}
/// Explicit one-time authority from the target's persistent-storage owner.
/// Ordinary agent startup and every validation path are non-creating.
pub fn initialize_persistent_home(home: &Path, environment: &Value) -> Result<(), FabricError> {
    let env_id: fabric_domain::EnvironmentId = id(environment,"environment_id")?.parse()?;
    directory(home)?;
    directory(&home.join("environments"))?;
    initialize_home(&home.join("environments").join(env_id.as_str()),environment)
}
fn directory(path: &Path) -> Result<(), FabricError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (), Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (), Err(e) => return Err(error(e)),
    }
    let meta = fs::symlink_metadata(path).map_err(error)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 { return Err(error("unsafe root")); }
    Ok(())
}
fn lock(path: &Path, exclusive: bool) -> Result<File, FabricError> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY).open(path).map_err(error)?;
    if unsafe { libc::flock(file.as_raw_fd(), (if exclusive {libc::LOCK_EX} else {libc::LOCK_SH}) | libc::LOCK_NB) } != 0 { return Err(error("resource active")); }
    Ok(file)
}
fn private_read(path: &Path, limit: u64) -> Result<Vec<u8>, FabricError> {
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(error)?;
    let meta = file.metadata().map_err(error)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe {libc::geteuid()} || meta.mode() & 0o077 != 0 || meta.len() > limit { return Err(error("unsafe private record")); }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes).map_err(error)?;
    if bytes.len() as u64 > limit { return Err(error("record bound")); }
    Ok(bytes)
}
fn id(value: &Value, key: &str) -> Result<String, FabricError> {
    let value = value[key].as_str().ok_or_else(|| error("identifier missing"))?;
    if value.is_empty() || value.len() > 128 || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') { return Err(error("invalid identifier")); }
    Ok(value.into())
}
fn hostname() -> Result<String, FabricError> {
    let mut bytes = [0u8; 256];
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 { return Err(error("hostname unavailable")); }
    let end = bytes.iter().position(|b| *b == 0).ok_or_else(|| error("hostname bounds"))?;
    String::from_utf8(bytes[..end].to_vec()).map_err(error)
}
struct Worker { token: CancellationToken, result: Arc<Mutex<Option<Result<Value, FabricError>>>>, thread: Option<std::thread::JoinHandle<()>> }
impl Drop for Worker {
    fn drop(&mut self) { self.token.cancel(); if let Some(thread) = self.thread.take() { let _ = thread.join(); } }
}
struct Agent {
    home: PathBuf, work: PathBuf, environment: Value, instance: String,
    pty: Option<fabric_runner::pty::LocalPty>, worker: Option<Worker>, destroy: Option<File>,
    export: Option<(Value, File, File)>,
}
impl Agent {
    fn env_id(&self) -> Result<fabric_domain::EnvironmentId, FabricError> { id(&self.environment, "environment_id")?.parse() }
    fn home(&self) -> Result<PathBuf, FabricError> { Ok(self.home.join("environments").join(self.env_id()?.as_str()).join("tree")) }
    fn validate_environment(&self, value: &Value) -> Result<(), FabricError> {
        if value != &self.environment { return Err(error("environment binding changed")); }
        self.validate_home()
    }
    fn validate_home(&self) -> Result<(), FabricError> {
        let home = self.home()?;
        root_identity(&self.home)?;
        root_identity(&self.home.join("environments"))?;
        let container = home.parent().unwrap();
        let recorded: HomeOwner = serde_json::from_slice(&private_read(&container.join(".fabric-owner"), 8192)?).map_err(error)?;
        let current = home_owner(container, &self.environment)?;
        if recorded.environment != self.environment || recorded.container != current.container || recorded.roots != current.roots {
            return Err(error("home binding or root identity changed"));
        }
        Ok(())
    }
    fn resource(&self, input: &Value) -> Result<(Value, PathBuf), FabricError> {
        let resource = input["resource"].clone();
        let run = id(&resource, "run_id")?;
        if resource["environment"] != self.environment || resource["instance"] != self.instance
            || resource["resource_id"] != format!("{}:{}:{}", self.environment["resource"]["resource_id"].as_str().ok_or_else(|| error("binding absent"))?, self.instance, run) { return Err(error("workspace binding changed")); }
        let container = self.work.join(&run);
        let bytes = private_read(&container.join(".fabric-owner"), 16384)?;
        if bytes != serde_json::to_vec(&resource).map_err(error)? { return Err(error("workspace foreign")); }
        Ok((resource, container))
    }
    fn adapter(input: &Value) -> Result<Box<dyn HarnessAdapter>, FabricError> {
        let harness: HarnessId = serde_json::from_value(input["harness"].clone()).map_err(error)?;
        fabric_runner::HarnessRegistry::default().resolve(&harness, &fabric_runner::RegistryOptions::default())
    }
    fn call(&mut self, operation: &str, input: Value) -> Result<Value, FabricError> {
        if self.destroy.is_some() { return Err(error("destroy in progress")); }
        match operation {
            "agent.identity" => Ok(json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"hostname":hostname()?,"instance":self.instance,"environment":self.environment})),
            "environment.inspect" | "environment.ensure" => { self.validate_environment(&input)?; Ok(json!({"available":true,"resource":self.environment["resource"]})) }
            "environment.prepare_destroy" => {
                self.validate_environment(&input)?;
                self.destroy = Some(lock(&self.home.join("environments"), true)?);
                Ok(json!({"exclusive":true}))
            }
            "harness.probe" | "harness.plan" => {
                self.validate_environment(&input["environment"])?;
                let _lease = lock(&self.home.join("environments"), false)?;
                let adapter = Self::adapter(&input)?;
                let info = adapter.probe()?;
                if operation == "harness.probe" { return Ok(json!({"version":info.version})); }
                if input["input"]["version"] != info.version { return Err(error("harness changed")); }
                let request: fabric_domain::HarnessRequest = serde_json::from_value(input["input"]["request"].clone()).map_err(error)?;
                let (plan, stamp) = crate::onboarding::plan(&self.home()?.join(".fabric"), adapter.as_ref(), request.clone(), &info)?;
                {
                    use sha2::{Digest, Sha256};
                    let key = format!("{:x}", Sha256::digest(serde_json::to_vec(&plan).map_err(error)?));
                    let path = self.home()?.join(format!(".fabric-plan-{key}"));
                    let receipt = serde_json::to_vec(&json!({"request":request,"stamp":stamp})).map_err(error)?;
                    if path.exists() {
                        if private_read(&path, 65536)? != receipt { return Err(error("accepted consent changed")); }
                    } else {
                        let mut file = remote::private_file(&path)?;
                        file.write_all(&receipt).and_then(|()|file.sync_all()).map_err(error)?;
                    }
                }
                serde_json::to_value(plan).map_err(error)
            }
            "pty.open" => {
                self.validate_environment(&input["environment"])?;
                if self.pty.is_some() { return Err(error("PTY already open")); }
                let home = self.home()?;
                let namespace = self.home.join("environments");
                let request = fabric_runner::pty::OpenExec {
                    version:1, placement:"remote.v1".into(), environment_id:self.env_id()?,
                    owner:id(&self.environment, "owner")?, owner_uid:unsafe {libc::geteuid()},
                    home_identity:fabric_runner::pty::Identity::of(&fs::symlink_metadata(&home).map_err(error)?),
                    namespace_identity:fabric_runner::pty::Identity::of(&fs::symlink_metadata(&namespace).map_err(error)?),
                    home, size:serde_json::from_value(input["size"].clone()).map_err(error)?,
                    limits:serde_json::from_value(input["limits"].clone()).map_err(error)?,
                };
                self.pty = Some(fabric_runner::pty::LocalPty::open(&request)?);
                Ok(json!({"opened":true}))
            }
            "pty.exchange" => {
                self.validate_home()?;
                let request = serde_json::from_value(input).map_err(error)?;
                serde_json::to_value(self.pty.as_mut().ok_or_else(|| error("no PTY"))?.exchange(&request)?).map_err(error)
            }
            "pty.close" => serde_json::to_value(self.pty.as_mut().ok_or_else(|| error("no PTY"))?.close(input["cancel"] == true)?).map_err(error),
            "workspace.create" => {
                self.validate_environment(&input["environment"])?;
                let run = id(&input, "run_id")?;
                let size = input["size"].as_u64().filter(|n| *n <= remote::TRANSFER_LIMIT).ok_or_else(|| error("source size"))?;
                let resource = json!({"resource_id":format!("{}:{}:{}",self.environment["resource"]["resource_id"].as_str().unwrap(),self.instance,run),"run_id":run,"instance":self.instance,"environment":self.environment});
                let container = self.work.join(&run);
                fs::DirBuilder::new().mode(0o700).create(&container).map_err(error)?;
                let mut marker = remote::private_file(&container.join(".fabric-owner"))?;
                marker.write_all(&serde_json::to_vec(&resource).map_err(error)?).and_then(|()| marker.sync_all()).map_err(error)?;
                let mut transfer = remote::private_file(&container.join("transfer.json"))?;
                transfer.write_all(&serde_json::to_vec(&json!({"size":size,"sha256":input["sha256"],"limit":input["limit"]})).map_err(error)?).map_err(error)?;
                remote::private_file(&container.join("source.tar"))?;
                Ok(resource)
            }
            "source.write" => {
                let (_, container) = self.resource(&input)?;
                if container.join("witness.json").exists() { return Err(error("source already sealed")); }
                let bytes: Vec<u8> = serde_json::from_value(input["bytes"].clone()).map_err(error)?;
                if bytes.is_empty() || bytes.len() > remote::CHUNK { return Err(error("source chunk")); }
                let mut file = OpenOptions::new().append(true).custom_flags(libc::O_NOFOLLOW).open(container.join("source.tar")).map_err(error)?;
                let offset = file.metadata().map_err(error)?.len();
                if input["offset"].as_u64() != Some(offset) || offset + bytes.len() as u64 > remote::TRANSFER_LIMIT { return Err(error("source offset")); }
                file.write_all(&bytes).map_err(error)?;
                Ok(json!({"offset":offset + bytes.len() as u64}))
            }
            "source.seal" => {
                let (resource, container) = self.resource(&input)?;
                let transfer: Value = serde_json::from_slice(&private_read(&container.join("transfer.json"), 16384)?).map_err(error)?;
                let path = container.join("source.tar");
                if fs::metadata(&path).map_err(error)?.len() != transfer["size"].as_u64().ok_or_else(|| error("source size"))?
                    || remote::digest(&path)? != transfer["sha256"].as_str().ok_or_else(|| error("source hash"))? { return Err(error("source mismatch")); }
                remote::unpack(File::open(&path).map_err(error)?, &container, transfer["limit"].as_u64().ok_or_else(|| error("source limit"))?)?;
                let actual_sha256 = fabric_project::prepare::content_digest(&container.join("tree"), Instant::now() + Duration::from_secs(1200))?;
                let witness = json!({"resource":resource,"actual_sha256":actual_sha256,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"hostname":hostname()?,"instance":self.instance});
                let mut file = remote::private_file(&container.join("witness.json"))?;
                file.write_all(&serde_json::to_vec(&witness).map_err(error)?).and_then(|()| file.sync_all()).map_err(error)?;
                fs::remove_file(path).map_err(error)?;
                Ok(witness)
            }
            "worker.start" => {
                let (resource, container) = self.resource(&input)?;
                if self.worker.is_some() || container.join("result.json").exists() { return Err(error("worker already started")); }
                self.validate_home()?;
                let home = self.home()?;
                let lease = lock(&self.home.join("environments"), false)?;
                let witness: Value = serde_json::from_slice(&private_read(&container.join("witness.json"), 16384)?).map_err(error)?;
                let actual = fabric_project::prepare::content_digest(&container.join("tree"), Instant::now() + Duration::from_secs(1200))?;
                if witness["actual_sha256"] != actual || input["actual_sha256"] != actual { return Err(error("prelaunch source changed")); }
                let plan: HarnessPlan = serde_json::from_value(input["plan"].clone()).map_err(error)?;
                if remote::digest(&container.join("prompt.input"))? != input["prompt_sha256"].as_str().ok_or_else(|| error("prompt hash absent"))? {
                    return Err(error("prompt changed"));
                }
                let workspace_lease = lock(&container, false)?;
                let adapter = fabric_runner::HarnessRegistry::default().resolve(&plan.manifest.harness_id, &fabric_runner::RegistryOptions::default())?;
                let info = adapter.probe()?;
                use sha2::{Digest, Sha256};
                let key = format!("{:x}", Sha256::digest(serde_json::to_vec(&plan).map_err(error)?));
                let consent = home.join(format!(".fabric-plan-{key}"));
                let receipt: Value = serde_json::from_slice(&private_read(&consent, 65536)?).map_err(error)?;
                let request = serde_json::from_value(receipt["request"].clone()).map_err(error)?;
                let (accepted, stamp) = crate::onboarding::plan(&home.join(".fabric"), adapter.as_ref(), request, &info)?;
                if serde_json::to_value(stamp).map_err(error)? != receipt["stamp"] { return Err(error("consent changed")); }
                if info.version != plan.manifest.exact_version || serde_json::to_value(accepted).map_err(error)? != serde_json::to_value(&plan).map_err(error)? { return Err(error("accepted harness plan changed")); }
                let limits = fabric_runner::ExecutionLimits { timeout:Duration::from_secs(input["timeout_seconds"].as_u64().ok_or_else(|| error("timeout"))?),term_grace:Duration::from_millis(input["term_grace_ms"].as_u64().ok_or_else(|| error("grace"))?) };
                limits.validate()?;
                let max_log = input["max_log_bytes"].as_u64().filter(|n| *n <= remote::TRANSFER_LIMIT).ok_or_else(|| error("log limit"))?;
                let storage = crate::workspace::storage::StorageOptions {
                    policy:serde_json::from_value(input["storage_policy"].clone()).map_err(error)?,
                    scan_timeout:Duration::from_millis(input["scan_timeout_ms"].as_u64().filter(|n| *n > 0 && *n <= 30_000).ok_or_else(||error("scan budget"))?),
                    ..Default::default()
                };
                let work_root = self.work.clone();
                let mut command = adapter.command(&fabric_runner::AgentSpec { profile:&plan.profile,prompt_file:&container.join("prompt.input"),max_cost_usd:plan.request.max_cost_usd,request:&plan.request })?;
                if command.environment.iter().any(|(key,_)| key == "HOME" || key.to_string_lossy().starts_with("XDG_")) { return Err(error("external HOME")); }
                for (key, suffix) in [("HOME",""),("XDG_CONFIG_HOME",".config"),("XDG_CACHE_HOME",".cache"),("XDG_DATA_HOME",".local/share"),("XDG_STATE_HOME",".local/state")] { command.environment.push((key.into(),home.join(suffix).into_os_string())); }
                command.environment.push(("FABRIC_EXECUTION_INSTANCE".into(),self.instance.clone().into()));
                let result = Arc::new(Mutex::new(None));
                let output = Arc::clone(&result);
                let token = CancellationToken::default(); let cancellation = token.clone();
                let (started_tx, started_rx) = mpsc::sync_channel(1);
                let harness_id = plan.manifest.harness_id.clone();
                let thread = std::thread::spawn(move || {
                    let outcome = (|| {
                        let _workspace_lease = workspace_lease;
                        let adapter = fabric_runner::HarnessRegistry::default().resolve(&harness_id, &fabric_runner::RegistryOptions::default())?;
                        let capture = fabric_runner::capture::Capture::new(remote::private_file(&container.join("stdout.log"))?,remote::private_file(&container.join("stderr.log"))?,Some(remote::private_file(&container.join("events.jsonl"))?),max_log).map_err(error)?.with_harness(adapter.as_ref());
                        let done = std::sync::atomic::AtomicBool::new(false);
                        let mut execution = std::thread::scope(|scope| {
                            let monitor = scope.spawn(|| crate::workspace::storage::monitor(
                                &work_root, &container.join("tree"), &storage, &cancellation, &done));
                            let execution = fabric_runner::execute_started(fabric_runner::ExecutionRequest {
                                spec:&command, cwd:&container.join("tree"), capture, limits:&limits,
                                cancellation:&cancellation, owner_lease:Some(&lease),
                            }, |_| { started_tx.send(()).map_err(error) });
                            done.store(true,std::sync::atomic::Ordering::Release);
                            monitor.thread().unpark();
                            monitor.join().map_err(|_|error("storage monitor"))?;
                            execution
                        })?;
                        if execution.stop_reason.is_none() {
                            execution.stop_reason = cancellation.storage_reason().or_else(|| {
                                match crate::workspace::storage::violation(&work_root,&container.join("tree"),&storage) {
                                    Ok(reason) => reason,
                                    Err(_) => Some(fabric_runner::StopReason::StorageMeterFailed),
                                }
                            });
                        }
                        let value = json!({"resource":resource,"exit_code":execution.status.code(),"exit_signal":execution.status.signal(),"stop_reason":execution.stop_reason.map(|r|r.as_str()),"harness_failed":execution.failure.is_some(),"capture":execution.capture.map_err(error)?});
                        let mut file = remote::private_file(&container.join("result.json"))?;
                        file.write_all(&serde_json::to_vec(&value).map_err(error)?).and_then(|()|file.sync_all()).map_err(error)?;
                        File::open(&container).and_then(|f|f.sync_all()).map_err(error)?;
                        Ok(value)
                    })();
                    *output.lock().unwrap_or_else(|p|p.into_inner()) = Some(outcome);
                });
                self.worker = Some(Worker {token,result,thread:Some(thread)});
                started_rx.recv_timeout(Duration::from_secs(30)).map_err(error)?;
                Ok(json!({"started":true}))
            }
            "worker.poll" | "worker.cancel" => {
                if operation == "worker.poll" { self.validate_home()?; }
                let (_, container) = self.resource(&input)?;
                if let Some(worker) = &self.worker {
                    if operation == "worker.cancel" { worker.token.cancel(); }
                    if let Some(outcome) = &*worker.result.lock().unwrap_or_else(|p|p.into_inner()) {
                        return match outcome { Ok(v) => Ok(json!({"state":"complete","result":v})),Err(_) => Err(error("remote harness execution failed")) };
                    }
                    Ok(json!({"state":"running"}))
                } else {
                    let _inactive = lock(&container,true)?;
                    let result: Value = serde_json::from_slice(&private_read(&container.join("result.json"), 65536)?).map_err(error)?;
                    Ok(json!({"state":"complete","result":result}))
                }
            }
            "workspace.export" => {
                self.validate_home()?;
                let (resource, container) = self.resource(&input)?;
                let result: Value = serde_json::from_slice(&private_read(&container.join("result.json"), 65536)?).map_err(error)?;
                remote::completion(result, &resource)?;
                if self.worker.as_ref().is_some_and(|w| w.result.lock().unwrap_or_else(|p|p.into_inner()).is_none()) { return Err(error("worker active")); }
                let lease = lock(&container, true)?;
                // Unlinked private staging: inspection never adds retained evidence.
                let path = self.work.join(format!(".export-{}", id(&resource,"run_id")?));
                let output = remote::private_file(&path)?;
                fs::remove_file(&path).map_err(error)?;
                let names = ["tree","stdout.log","stderr.log","events.jsonl"];
                let paths = names.map(|n|container.join(n));
                let entries: [(&Path,&Path);4] = [(&paths[0],Path::new(names[0])),(&paths[1],Path::new(names[1])),
                    (&paths[2],Path::new(names[2])),(&paths[3],Path::new(names[3]))];
                let mut output = remote::archive(&entries,output,input["limit"].as_u64().ok_or_else(|| error("export limit"))?)?;
                output.seek(SeekFrom::Start(0)).map_err(error)?;
                use sha2::{Digest, Sha256};
                let mut hash = Sha256::new();
                let mut buffer = [0;65536];
                loop { let n = output.read(&mut buffer).map_err(error)?; if n == 0 { break; } hash.update(&buffer[..n]); }
                let witness: Value = serde_json::from_slice(&private_read(&container.join("witness.json"),16384)?).map_err(error)?;
                if witness["resource"] != resource { return Err(error("source witness changed")); }
                let reply = json!({"size":output.metadata().map_err(error)?.len(),"sha256":format!("{:x}",hash.finalize()),"witness":witness});
                self.export = Some((resource,output,lease));
                Ok(reply)
            }
            "workspace.read" => {
                let (resource, _) = self.resource(&input)?;
                let (bound, file, _) = self.export.as_mut().ok_or_else(||error("no export"))?;
                if bound != &resource { return Err(error("export binding changed")); }
                let offset = input["offset"].as_u64().ok_or_else(|| error("offset"))?;
                let size = file.metadata().map_err(error)?.len();
                if offset >= size { return Err(error("export offset")); }
                file.seek(SeekFrom::Start(offset)).map_err(error)?;
                let mut bytes = vec![0;remote::CHUNK]; let n = file.read(&mut bytes).map_err(error)?; bytes.truncate(n);
                if offset + n as u64 == size { self.export = None; }
                Ok(json!({"bytes":bytes}))
            }
            "workspace.reclaim" => {
                let resource = &input["resource"];
                let run = id(resource, "run_id")?;
                let tombstone = self.work.join(format!(".reclaimed-{run}"));
                if tombstone.exists() {
                    let saved: Value = serde_json::from_slice(&private_read(&tombstone, 16384)?).map_err(error)?;
                    if &saved != resource { return Err(error("reclaim binding changed")); }
                    if !self.work.join(&run).exists() { return Ok(json!({"reclaimed":true})); }
                }
                let (resource, container) = self.resource(&input)?;
                if self.worker.as_ref().is_some_and(|w| w.result.lock().unwrap_or_else(|p|p.into_inner()).is_none()) { return Err(error("worker active")); }
                let _lease = lock(&container, true)?;
                if !tombstone.exists() {
                    let mut file = remote::private_file(&tombstone)?;
                    file.write_all(&serde_json::to_vec(&resource).map_err(error)?).and_then(|()|file.sync_all()).map_err(error)?;
                }
                fs::remove_dir_all(container).map_err(error)?;
                Ok(json!({"reclaimed":true}))
            }
            _ => Err(error("unsupported agent operation")),
        }
    }
}

/// The target transports NDJSON only. The actual PTY and process group are created
/// here, in the execution environment, and reaped on transport EOF or heartbeat loss.
pub fn serve(home: PathBuf, work: PathBuf, environment: Value, instance: String) -> Result<(), FabricError> {
    if !home.is_absolute() || !work.is_absolute() || home == work || instance.is_empty() || instance.len() > 128 { return Err(error("agent roots")); }
    directory(&work)?;
    let env_id: fabric_domain::EnvironmentId = id(&environment,"environment_id")?.parse()?;
    let container = home.join("environments").join(env_id.as_str());
    let tree = container.join("tree");
    root_identity(&home)?;
    root_identity(&home.join("environments"))?;
    let mut agent = Agent {home,work,environment,instance,pty:None,worker:None,destroy:None,export:None};
    agent.validate_home()?;
    // Set only the remote agent's roots, before any thread is created. Native
    // adapters resolve their own installed context here; no controller auth input.
    unsafe {
        std::env::set_var("HOME", &tree);
        for (key,path) in [("XDG_CONFIG_HOME",".config"),("XDG_CACHE_HOME",".cache"),("XDG_DATA_HOME",".local/share"),("XDG_STATE_HOME",".local/state")] { std::env::set_var(key, tree.join(path)); }
    }
    let (tx,rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let mut bytes = Vec::new();
            let result = input.by_ref().take(65537).read_until(b'\n', &mut bytes);
            if result.is_err() || bytes.is_empty() || bytes.len() > 65536 || !bytes.ends_with(b"\n") { break; }
            if tx.send(bytes).is_err() { return; }
        }
    });
    let mut heartbeat = Instant::now();
    let mut expected = 1u64;
    loop {
        if let Some(pty) = &mut agent.pty { pty.tick()?; }
        let bytes = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(bytes) => bytes,
            Err(mpsc::RecvTimeoutError::Timeout) if heartbeat.elapsed() < Duration::from_secs(10) => continue,
            Err(_) => break,
        };
        let request: Value = serde_json::from_slice(&bytes).map_err(error)?;
        let sequence = request["id"].as_u64().ok_or_else(|| error("request id"))?;
        if sequence != expected || request.as_object().is_none_or(|v| v.len() != 3 || !v.contains_key("operation") || !v.contains_key("input")) {
            return Err(error("request sequence"));
        }
        expected = expected.checked_add(1).ok_or_else(|| error("request sequence overflow"))?;
        let result = if request["operation"] == "heartbeat" { Ok(json!({"alive":true})) }
            else { agent.call(request["operation"].as_str().ok_or_else(|| error("operation"))?,request["input"].clone()) };
        let reply = match result { Ok(result) => json!({"id":sequence,"result":result}), Err(failure) => json!({"id":sequence,"error":{"kind":failure.kind}}) };
        let mut bytes = serde_json::to_vec(&reply).map_err(error)?; bytes.push(b'\n');
        if bytes.len() > 65536 { return Err(error("reply bounds")); }
        let mut output = std::io::stdout().lock(); output.write_all(&bytes).and_then(|()|output.flush()).map_err(error)?;
        heartbeat = Instant::now();
    }
    drop(agent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> (tempfile::TempDir,Agent) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let work = temp.path().join("work");
        directory(&work).unwrap();
        let environment = json!({"environment_id":"env-roots","owner":"owned",
            "resource":{"resource_id":"persistent-storage-original"}});
        initialize_persistent_home(&home,&environment).unwrap();
        let agent = Agent {home,work,environment,instance:"first-pod".into(),
            pty:None,worker:None,destroy:None,export:None};
        agent.validate_home().unwrap();
        (temp,agent)
    }
    #[test]
    fn home_and_each_xdg_root_are_never_recreated_or_adopted() {
        for root in HOME_ROOTS {
            for replacement in ["absent","directory","symlink"] {
                let (temp,agent) = fixture();
                let container = agent.home().unwrap().parent().unwrap().to_path_buf();
                let marker = fs::read(container.join(".fabric-owner")).unwrap();
                let home = agent.home().unwrap();
                let path = if root.is_empty() { home } else { home.join(root) };
                let retained = temp.path().join("retained-original");
                fs::rename(&path,&retained).unwrap();
                if replacement == "directory" {
                    directory(&path).unwrap();
                    // Restore the original child roots; only the selected root
                    // identity differs, with unchanged private ownership bytes.
                    for child in fs::read_dir(&retained).unwrap() {
                        let child = child.unwrap();
                        fs::rename(child.path(),path.join(child.file_name())).unwrap();
                    }
                } else if replacement == "symlink" {
                    std::os::unix::fs::symlink(&retained,&path).unwrap();
                }
                assert!(agent.validate_home().is_err(),"{root}: {replacement}");
                assert_eq!(fs::read(container.join(".fabric-owner")).unwrap(),marker);
                if replacement == "absent" { assert!(fs::symlink_metadata(&path).is_err()); }
                // Read-only environment operations cannot repair validation.
                assert!(agent.validate_environment(&agent.environment).is_err());
                if replacement == "absent" { assert!(fs::symlink_metadata(&path).is_err()); }
            }
        }
    }
    #[test]
    fn unchanged_restart_and_same_storage_relocation_preserve_home_not_instance() {
        let (temp,mut agent) = fixture();
        let home = agent.home().unwrap();
        fs::write(home.join("retained"),b"persistent config").unwrap();
        // Native auth contents are not inspected, copied, or dereferenced.
        std::os::unix::fs::symlink(temp.path().join("never-accessed"),home.join(".config/native-auth")).unwrap();
        agent.instance = "replacement-pod".into();
        agent.validate_home().unwrap();
        let moved = temp.path().join("same-storage");
        fs::rename(&agent.home,&moved).unwrap();
        agent.home = moved;
        agent.validate_home().unwrap();
        assert_eq!(fs::read(agent.home().unwrap().join("retained")).unwrap(),b"persistent config");
        assert!(initialize_persistent_home(&agent.home,&agent.environment).is_err());
    }
    #[test]
    fn incomplete_tampered_or_unsafe_home_records_refuse() {
        for corruption in ["legacy","tampered","permissions"] {
            let (_temp,agent) = fixture();
            let marker = agent.home().unwrap().parent().unwrap().join(".fabric-owner");
            if corruption == "permissions" {
                fs::set_permissions(agent.home().unwrap(),fs::Permissions::from_mode(0o755)).unwrap();
            } else {
                let mut value: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
                if corruption == "legacy" { value = json!({"environment":agent.environment}); }
                else { value["roots"][0]["inode"] = json!(0); }
                fs::write(&marker,serde_json::to_vec(&value).unwrap()).unwrap();
            }
            assert!(agent.validate_home().is_err());
        }
    }
    #[test]
    fn removed_persistent_container_or_parent_is_not_recreated_by_restart() {
        for parent in ["container","environments","home"] {
            let (temp,agent)=fixture();
            let removed=match parent {
                "container"=>agent.home().unwrap().parent().unwrap().to_owned(),
                "environments"=>agent.home.join("environments"),
                _=>agent.home.clone(),
            };
            fs::rename(&removed,temp.path().join("retained")).unwrap();
            assert!(agent.validate_home().is_err());
            assert!(serve(agent.home.clone(),agent.work.clone(),agent.environment.clone(),"replacement".into()).is_err());
            assert!(fs::symlink_metadata(removed).is_err());
        }
    }
    #[test]
    fn shared_worker_and_shell_home_leases_exclude_explicit_destroy() {
        for _owner in ["worker","shell"] {
            let (_temp,mut agent)=fixture();
            let owner=lock(&agent.home.join("environments"),false).unwrap();
            assert!(agent.call("environment.prepare_destroy",agent.environment.clone()).is_err());
            assert!(agent.destroy.is_none());
            drop(owner);
            assert_eq!(agent.call("environment.prepare_destroy",agent.environment.clone()).unwrap(),json!({"exclusive":true}));
        }
    }
}
