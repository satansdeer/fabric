//! A single trusted-local supervisor. SQLite owns admission; only live tokens own cancellation.
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fabric_domain::{BatchId, ErrorKind, FabricError, HarnessId, HarnessPlan, RunId, RunState};
use fabric_runner::{CancellationToken, HarnessRegistry, RegistryOptions};
use fabric_store::{AsyncBatch, AsyncTaskStatus, NewAsyncTask, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::api::{BatchRequest, Options, WorkerBatchRequest};
use crate::run::{BatchHarnesses, ExecutionBatch, RunReport};
use crate::workspace::open_store;

const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_CLIENTS: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Submit {
        request: BatchRequest,
        fake_binary: Option<PathBuf>,
        idempotency_key: Option<String>,
    },
    Spawn {
        request: WorkerBatchRequest,
        fake_binary: Option<PathBuf>,
        idempotency_key: Option<String>,
    },
    Status {
        id: String,
        after: u64,
    },
    Results {
        id: String,
    },
    Cancel {
        id: String,
    },
    Wait {
        id: String,
        timeout_seconds: u64,
    },
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    root: RootIdentity,
    request: Request,
}

struct Endpoint {
    root: RootIdentity,
    directory: PathBuf,
    socket: PathBuf,
}

impl Endpoint {
    fn new(root: &Path) -> Result<Self, FabricError> {
        let (root, _) = open_store(root)?;
        let metadata = fs::symlink_metadata(&root).map_err(error)?;
        let identity = RootIdentity {
            path: root,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&identity).map_err(error)?)
        );
        // /tmp is deliberately short: Darwin's sockaddr_un has only 104 path bytes.
        let directory = Path::new("/tmp")
            .canonicalize()
            .map_err(error)?
            .join(format!(
                "fabric-{}-{}",
                unsafe { libc::geteuid() },
                &digest[..24],
            ));
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(failure) => return Err(error(failure)),
        }
        crate::workspace::private_directory(&directory)?;
        let socket = directory.join("manager.sock");
        if socket.as_os_str().as_encoded_bytes().len() >= 104 {
            return Err(error("Local manager socket path exceeds the macOS limit."));
        }
        Ok(Self {
            root: identity,
            directory,
            socket,
        })
    }

    fn validate_socket(&self) -> Result<(), FabricError> {
        crate::workspace::private_directory(&self.directory)?;
        let metadata = fs::symlink_metadata(&self.socket).map_err(error)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(error(
                "Manager endpoint must be an owner-only owned Unix socket.",
            ));
        }
        Ok(())
    }

    fn connect(&self) -> Result<UnixStream, FabricError> {
        self.validate_socket()?;
        UnixStream::connect(&self.socket).map_err(error)
    }
}

/// The first submit starts a detached copy of this executable, not a foreground child worker.
pub fn request(root: &Path, request: Request) -> Result<Value, FabricError> {
    if let Request::Spawn {
        request,
        fake_binary,
        ..
    } = &request
    {
        crate::api::preflight_workers(
            root,
            request,
            &RegistryOptions {
                fake_binary: fake_binary.clone(),
            },
        )?;
    }
    let endpoint = Endpoint::new(root)?;
    let wait = match &request {
        Request::Wait {
            timeout_seconds, ..
        } => *timeout_seconds,
        _ => 0,
    };
    if wait > 3600 {
        return Err(invalid("Wait timeout must be at most 3600 seconds."));
    }
    let stop = matches!(request, Request::Stop);
    let stream = match endpoint.connect() {
        Ok(stream) => stream,
        Err(failure) => {
            // Reject unsafe named entries instead of replacing or following them.
            if fs::symlink_metadata(&endpoint.socket).is_ok() {
                endpoint.validate_socket()?;
            }
            if stop {
                return Ok(json!({"manager_stopped":true}));
            }
            start_detached(&endpoint.root.path)?;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Ok(stream) = endpoint.connect() {
                    break stream;
                }
                if Instant::now() >= deadline {
                    return Err(error(format!(
                        "Local manager did not become ready: {failure}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(wait + 20)))
        .map_err(error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(15)))
        .map_err(error)?;
    let mut stream = stream;
    // A failed reply is not automatically retried: acceptance may already be committed.
    write_frame(
        &mut stream,
        &Envelope {
            version: 1,
            root: endpoint.root,
            request,
        },
    )?;
    let response: Value = read_frame(&mut stream)?;
    if let Some(failure) = response.get("error") {
        return Err(serde_json::from_value::<FabricError>(failure.clone())
            .map_err(|_| error("Invalid typed local manager error."))?);
    }
    response
        .get("ok")
        .cloned()
        .ok_or_else(|| error("Invalid local manager response."))
}

fn start_detached(root: &Path) -> Result<(), FabricError> {
    let mut command = Command::new(std::env::current_exe().map_err(error)?);
    command
        .arg("--root")
        .arg(root)
        .args(["manager", "serve", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no Rust state in the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(error)?;
    // Reap if the client happens to remain alive; exiting the client does not kill the manager.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[derive(Default)]
struct Control {
    tokens: HashMap<RunId, CancellationToken>,
    batches: HashSet<BatchId>,
    pending: VecDeque<BatchId>,
    generation: u64,
    stopping: bool,
}

struct Manager {
    root: PathBuf,
    control: Mutex<Control>,
    changed: Condvar,
}

struct OwnedTask<'a> {
    manager: &'a Manager,
    run: &'a RunId,
}
impl Drop for OwnedTask<'_> {
    fn drop(&mut self) {
        let mut control = self
            .manager
            .control
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        control.tokens.remove(self.run);
        control.generation = control.generation.wrapping_add(1);
        self.manager.changed.notify_all();
    }
}

impl Manager {
    fn notify(&self) {
        let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
        control.generation = control.generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn dispatch(self: &Arc<Self>, request: Request) -> Result<Value, FabricError> {
        match request {
            Request::Submit {
                request,
                fake_binary,
                idempotency_key,
            } => {
                let digest = request_digest(&request, &fake_binary)?;
                let batch = crate::api::translate_v1(request, CancellationToken::default())?;
                self.submit(batch, fake_binary, idempotency_key, digest, false)
            }
            Request::Spawn {
                request,
                fake_binary,
                idempotency_key,
            } => {
                let digest = request_digest(&request, &fake_binary)?;
                let batch = crate::api::translate_workers(request)?;
                self.submit(batch, fake_binary, idempotency_key, digest, true)
            }
            Request::Status { id, after } => self.status(&id, after),
            Request::Results { id } => self.results(&id),
            Request::Cancel { id } => self.cancel(&id),
            Request::Wait {
                id,
                timeout_seconds,
            } => self.wait(&id, timeout_seconds),
            Request::Stop => {
                let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
                if !control.batches.is_empty() || !control.tokens.is_empty() {
                    return Err(invalid(
                        "Manager has active work; cancel or wait before stopping it.",
                    ));
                }
                control.stopping = true;
                control.generation = control.generation.wrapping_add(1);
                self.changed.notify_all();
                Ok(json!({"manager_stopped":true}))
            }
        }
    }

    fn submit(
        self: &Arc<Self>,
        request: ExecutionBatch,
        fake_binary: Option<PathBuf>,
        key: Option<String>,
        digest: String,
        generic: bool,
    ) -> Result<Value, FabricError> {
        if key
            .as_ref()
            .is_some_and(|key| key.trim().is_empty() || key.len() > 256)
        {
            return Err(invalid("Idempotency key must contain 1-256 bytes."));
        }
        if fake_binary.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(invalid("Async --fake-binary must be an absolute path."));
        }
        let (_, mut store) = open_store(&self.root)?;
        if let Some(prior) = key
            .as_deref()
            .map(|key| store.async_idempotency(key))
            .transpose()
            .map_err(error)?
            .flatten()
        {
            if prior.request_digest != digest {
                return Err(invalid(
                    "Idempotency key is already bound to a different request.",
                ));
            }
            self.schedule(prior.batch_id.clone())?;
            return accepted(&store, &prior.batch_id, generic);
        }
        let registry = HarnessRegistry::default();
        let registry_options = RegistryOptions {
            fake_binary: fake_binary.clone(),
        };
        let concurrency = request.concurrency.min(crate::api::MAX_TASKS);
        let prepared = crate::run::prepare_batch(
            &self.root,
            request,
            &BatchHarnesses {
                registry: &registry,
                options: &registry_options,
                default: None,
            },
        )?;
        let mut tasks = Vec::with_capacity(prepared.tasks.len());
        for ((task, (harness, (plan, stamp))), input) in prepared
            .tasks
            .into_iter()
            .zip(prepared.plans)
            .zip(prepared.inputs)
        {
            crate::run::validate_durable_plan(&plan)?;
            tasks.push(NewAsyncTask {
                task,
                prompt: input
                    .prompt
                    .ok_or_else(|| invalid("Durable tasks require inline prompts."))?,
                harness: harness.to_string(),
                plan_json: serde_json::to_string(&AcceptedPlan {
                    plan,
                    limits: Some(Options::from_run(&input.options)),
                })
                .map_err(error)?,
                consent_stamp: stamp,
            });
        }
        let batch = AsyncBatch {
            batch_id: BatchId::try_from(format!("b-{}", crate::run::new_run_id()?))?,
            prepared_state_id: prepared.state.state_id,
            request_digest: digest,
            concurrency,
            options_json: serde_json::to_string(&Options::default()).map_err(error)?,
            fake_binary: fake_binary
                .map(|path| {
                    path.into_os_string()
                        .into_string()
                        .map_err(|_| invalid("Fake binary path must be UTF-8."))
                })
                .transpose()?,
        };
        // Serialize stop/admission/claim/cancel, not execution. SQLite arbitrates duplicate clients.
        let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
        if control.stopping {
            return Err(invalid("Manager is stopping; reconnect and submit again."));
        }
        let id = store
            .accept_async_batch(&batch, key.as_deref(), &tasks, now()?)
            .map_err(|failure| {
                if matches!(failure, fabric_store::StoreError::Conflict) {
                    invalid("Idempotency key is already bound to a different request.")
                } else {
                    error(failure)
                }
            })?;
        Self::enqueue(&mut control, id.clone());
        self.changed.notify_all();
        drop(control);
        accepted(&store, &id, generic)
    }

    fn enqueue(control: &mut Control, batch: BatchId) {
        if control.batches.insert(batch.clone()) {
            control.pending.push_back(batch);
            control.generation = control.generation.wrapping_add(1);
        }
    }

    fn schedule(&self, batch: BatchId) -> Result<(), FabricError> {
        let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
        if control.stopping {
            return Err(invalid("Manager is stopping."));
        }
        Self::enqueue(&mut control, batch);
        self.changed.notify_all();
        Ok(())
    }

    fn run_scheduler(&self) {
        loop {
            let batch = {
                let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
                while control.pending.is_empty() && !control.stopping {
                    control = self
                        .changed
                        .wait(control)
                        .unwrap_or_else(|p| p.into_inner());
                }
                if control.stopping {
                    return;
                }
                control.pending.pop_front().expect("pending batch")
            };
            if let Err(failure) = self.execute_batch(&batch) {
                // All pool workers have joined; only uncompleted claims are now uncertain.
                let _ = self.recover_batch(&batch, Some(&failure.message));
            }
            let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
            control.batches.remove(&batch);
            control.generation = control.generation.wrapping_add(1);
            self.changed.notify_all();
        }
    }

    fn execute_batch(&self, id: &BatchId) -> Result<(), FabricError> {
        let (_, store) = open_store(&self.root)?;
        let batch = store
            .async_batch(id)
            .map_err(error)?
            .ok_or_else(|| error("Missing async batch."))?;
        let state = store
            .prepared_state(&batch.prepared_state_id)
            .map_err(error)?
            .ok_or_else(|| error("Missing accepted prepared state."))?;
        let tasks = store.async_tasks(id).map_err(error)?;
        let options: Options = serde_json::from_str(&batch.options_json).map_err(error)?;
        crate::run::run_pool(tasks.len(), batch.concurrency, |index| {
            let run = &tasks[index].run_id;
            let (_, mut store) = open_store(&self.root)?;
            let token = CancellationToken::default();
            {
                let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
                if !store.claim_async_task(run).map_err(error)? {
                    return Ok(());
                }
                control.tokens.insert(run.clone(), token.clone());
                control.generation = control.generation.wrapping_add(1);
                self.changed.notify_all();
            }
            let _ownership = OwnedTask { manager: self, run };
            let outcome = (|| {
                let task = store.async_task(run).map_err(error)?;
                let accepted: AcceptedPlan =
                    serde_json::from_str(&task.plan_json).map_err(error)?;
                let plan = accepted.plan;
                let registry = HarnessRegistry::default();
                let adapter = registry.resolve(
                    &HarnessId::try_from(task.harness)?,
                    &RegistryOptions {
                        fake_binary: batch.fake_binary.as_ref().map(PathBuf::from),
                    },
                )?;
                if adapter.probe()?.version != plan.manifest.exact_version {
                    return Err(invalid(
                        "Accepted harness version changed; task was not launched.",
                    ));
                }
                let options = crate::api::run_options(
                    accepted.limits.as_ref().unwrap_or(&options),
                    token.clone(),
                )?;
                crate::run::execute_prepared(
                    &self.root,
                    &state,
                    &task.prompt,
                    adapter.as_ref(),
                    &options,
                    &(plan, task.consent_stamp),
                    run.clone(),
                )
            })();
            let result = outcome
                .as_ref()
                .ok()
                .map(|report| serde_json::to_string(report).map_err(error))
                .transpose()?;
            let state = match &outcome {
                Ok(report) => report.state,
                Err(_) => store
                    .run(run)
                    .map_err(error)?
                    .map(|run| run.state)
                    .filter(|state| terminal(*state))
                    .unwrap_or(if token.is_cancelled() {
                        RunState::Cancelled
                    } else {
                        RunState::Failed
                    }),
            };
            store
                .complete_async_task(
                    run,
                    state,
                    match &result {
                        Some(result) => Ok(result.as_str()),
                        None => Err(outcome
                            .as_ref()
                            .err()
                            .map(|e| e.message.as_str())
                            .unwrap_or("Task failed.")),
                    },
                    false,
                )
                .map_err(error)?;
            Ok(())
        })
    }

    fn recover_batch(&self, id: &BatchId, failure: Option<&str>) -> Result<(), FabricError> {
        let (_, mut store) = open_store(&self.root)?;
        for task in store.async_tasks(id).map_err(error)? {
            if terminal(task.queue_state) {
                continue;
            }
            let run = store.run(&task.run_id).map_err(error)?;
            if task.queue_state == RunState::Queued && run.is_none() {
                continue;
            }
            // A committed terminal run is proof, even when the queue-result commit was interrupted.
            if let Some(run) = run
                .as_ref()
                .filter(|run| terminal(run.state) && run.state != RunState::NeedsAttention)
            {
                if let Some(report) = recovered_report(&self.root, &store, run)? {
                    let result = serde_json::to_string(&report).map_err(error)?;
                    store
                        .complete_async_task(&task.run_id, run.state, Ok(&result), false)
                        .map_err(error)?;
                    continue;
                } else {
                    store.complete_async_task(&task.run_id, run.state,
                        Err("Terminal run persisted before queue result; no complete capture report is available."), false).map_err(error)?;
                    continue;
                }
            }
            store
                .reconcile_interrupted(task.run_id.as_str(), false, now()?)
                .map_err(error)?;
            store.complete_async_task(&task.run_id, RunState::NeedsAttention,
                Err(failure.unwrap_or("Interrupted manager; worker ownership is unknown. Inspect retained workspace; task will not be retried.")), false).map_err(error)?;
        }
        self.notify();
        Ok(())
    }

    fn status(&self, id: &str, after: u64) -> Result<Value, FabricError> {
        let (_, store) = open_store(&self.root)?;
        let (batch, run, tasks) = selection(&store, id)?;
        let settled = tasks.iter().all(|task| terminal(task.queue_state));
        let state = aggregate(&tasks);
        let events = store
            .async_events(&batch, run.as_ref(), after)
            .map_err(error)?;
        let next = events.last().map_or(after, |event| event.cursor);
        Ok(
            json!({"id":id,"batch_id":batch,"state":state,"terminal":settled,
            "tasks":tasks.iter().map(|task| json!({"task_id":task.task_id,"run_id":task.run_id,"state":task.state,"terminal":terminal(task.queue_state)})).collect::<Vec<_>>(),
            "events":events,"next_cursor":next}),
        )
    }

    fn results(&self, id: &str) -> Result<Value, FabricError> {
        let (_, store) = open_store(&self.root)?;
        let (batch, _, tasks) = selection(&store, id)?;
        let settled = tasks.iter().all(|task| terminal(task.queue_state));
        let succeeded = settled.then(|| tasks.iter().all(|task| task.state == RunState::Succeeded));
        let tasks = tasks.into_iter().map(|task| {
            let result: Option<Value> = task.result.as_deref().map(serde_json::from_str).transpose().map_err(error)?;
            let artifacts = store.artifacts(&task.run_id).map_err(error)?;
            Ok(json!({"task_id":task.task_id,"run_id":task.run_id,"state":task.state,"result":result,"error":task.error,"artifacts":artifacts}))
        }).collect::<Result<Vec<_>,FabricError>>()?;
        Ok(json!({"batch_id":batch,"terminal":settled,"succeeded":succeeded,"tasks":tasks}))
    }

    fn cancel(&self, id: &str) -> Result<Value, FabricError> {
        let mut control = self.control.lock().unwrap_or_else(|p| p.into_inner());
        let (_, mut store) = open_store(&self.root)?;
        let (_, _, tasks) = selection(&store, id)?;
        let mut cancelled = false;
        let mut unowned = false;
        for task in tasks {
            if task.state == RunState::NeedsAttention {
                unowned = true;
                continue;
            }
            if terminal(task.state) {
                continue;
            }
            if task.queue_state == RunState::Queued
                && store
                    .complete_async_task(
                        &task.run_id,
                        RunState::Cancelled,
                        Err("Cancelled while queued; task was not launched."),
                        true,
                    )
                    .map_err(error)?
            {
                cancelled = true;
            } else if let Some(token) = control.tokens.get(&task.run_id) {
                token.cancel();
                cancelled = true;
            } else {
                unowned = true;
            }
        }
        control.generation = control.generation.wrapping_add(1);
        self.changed.notify_all();
        let outcome = if cancelled {
            "cancelled"
        } else if unowned {
            "not_owned"
        } else {
            "already_terminal"
        };
        Ok(json!({"id":id,"acknowledged":cancelled,"outcome":outcome}))
    }

    fn wait(&self, id: &str, seconds: u64) -> Result<Value, FabricError> {
        if seconds > 3600 {
            return Err(invalid("Wait timeout must be at most 3600 seconds."));
        }
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let control = self.control.lock().unwrap_or_else(|p| p.into_inner());
            let generation = control.generation;
            drop(control);
            let status = self.status(id, 0)?;
            if status["terminal"] == true || Instant::now() >= deadline {
                return Ok(status);
            }
            let control = self.control.lock().unwrap_or_else(|p| p.into_inner());
            if control.generation != generation {
                continue;
            }
            let _ = self
                .changed
                .wait_timeout_while(
                    control,
                    deadline.saturating_duration_since(Instant::now()),
                    |control| control.generation == generation && !control.stopping,
                )
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Foreground manager entry point. Readiness follows lock acquisition and recovery.
pub fn serve(
    root: &Path,
    ready: impl FnOnce(Value) -> Result<(), FabricError>,
) -> Result<(), FabricError> {
    let endpoint = Endpoint::new(root)?;
    let lock_path = endpoint.root.path.join("manager.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&lock_path)
        .map_err(error)?;
    crate::workspace::private_file(&lock_path)?;
    let actual = lock.metadata().map_err(error)?;
    let named = fs::symlink_metadata(&lock_path).map_err(error)?;
    if (actual.dev(), actual.ino()) != (named.dev(), named.ino()) || !actual.is_file() {
        return Err(error("Manager lock identity changed."));
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(error("A manager already owns this Fabric root."));
    }
    lock.sync_all().map_err(error)?;
    File::open(&endpoint.root.path)
        .and_then(|file| file.sync_all())
        .map_err(error)?;
    if fs::symlink_metadata(&endpoint.socket).is_ok() {
        endpoint.validate_socket()?;
        fs::remove_file(&endpoint.socket).map_err(error)?;
    }
    let listener = UnixListener::bind(&endpoint.socket).map_err(error)?;
    fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600)).map_err(error)?;
    let socket_identity = fs::symlink_metadata(&endpoint.socket).map_err(error)?;
    let _socket = SocketGuard {
        path: endpoint.socket.clone(),
        device: socket_identity.dev(),
        inode: socket_identity.ino(),
    };
    listener.set_nonblocking(true).map_err(error)?;
    let manager = Arc::new(Manager {
        root: endpoint.root.path.clone(),
        control: Mutex::new(Control::default()),
        changed: Condvar::new(),
    });
    let (_, store) = open_store(&manager.root)?;
    let batches = store.async_unfinished_batches().map_err(error)?;
    for batch in &batches {
        manager.recover_batch(batch, None)?;
    }
    for batch in batches {
        manager.schedule(batch)?;
    }
    // ponytail: one batch at a time; interleave batches only when fairness matters.
    let scheduler_manager = Arc::clone(&manager);
    let scheduler = std::thread::Builder::new()
        .name("fabric-scheduler".into())
        .spawn(move || scheduler_manager.run_scheduler())
        .map_err(error)?;
    ready(json!({"manager_ready":true,"root":endpoint.root.path,"protocol_version":1}))?;
    let clients = Arc::new(AtomicUsize::new(0));
    loop {
        if manager
            .control
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopping
        {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
                    clients.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let clients = Arc::clone(&clients);
                let manager = Arc::clone(&manager);
                let identity = endpoint.root.clone();
                std::thread::spawn(move || {
                    let _client = ClientGuard(clients);
                    let outcome = (|| {
                        // Darwin may inherit O_NONBLOCK from the listener; frame writes must finish.
                        stream.set_nonblocking(false).map_err(error)?;
                        stream
                            .set_read_timeout(Some(Duration::from_secs(15)))
                            .map_err(error)?;
                        stream
                            .set_write_timeout(Some(Duration::from_secs(15)))
                            .map_err(error)?;
                        let envelope: Envelope = read_frame(&mut stream)?;
                        if envelope.version != 1 || envelope.root != identity {
                            return Err(invalid("Manager protocol/root identity mismatch."));
                        }
                        let metadata = fs::symlink_metadata(&identity.path).map_err(error)?;
                        if metadata.dev() != identity.device || metadata.ino() != identity.inode {
                            return Err(error(
                                "Fabric root identity changed; manager will not use a replacement root.",
                            ));
                        }
                        crate::workspace::private_directory(&identity.path)?;
                        manager.dispatch(envelope.request)
                    })();
                    let reply = match outcome {
                        Ok(value) => json!({"ok":value}),
                        Err(failure) => json!({"error":failure}),
                    };
                    if let Err(failure) = write_frame(&mut stream, &reply) {
                        eprintln!("fabric manager response failed: {failure}");
                    }
                });
            }
            Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => {
                let control = manager.control.lock().unwrap_or_else(|p| p.into_inner());
                let _ = manager
                    .changed
                    .wait_timeout(control, Duration::from_millis(25))
                    .unwrap_or_else(|p| p.into_inner());
            }
            Err(failure) if failure.kind() == std::io::ErrorKind::Interrupted => {}
            Err(failure) => return Err(error(failure)),
        }
    }
    // Finish the stop acknowledgement before the foreground server exits.
    while clients.load(Ordering::Acquire) != 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    scheduler
        .join()
        .map_err(|_| error("Manager scheduler panicked."))?;
    Ok(())
}

struct ClientGuard(Arc<AtomicUsize>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| {
            m.dev() == self.device && m.ino() == self.inode && m.file_type().is_socket()
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn selection(
    store: &Store,
    id: &str,
) -> Result<(BatchId, Option<RunId>, Vec<AsyncTaskStatus>), FabricError> {
    let candidate = BatchId::try_from(id.to_owned())?;
    let (batch, run) = if store.async_batch(&candidate).map_err(error)?.is_some() {
        (candidate, None)
    } else {
        let run = RunId::try_from(id.to_owned())?;
        let batch = store
            .batch_for_run(&run)
            .map_err(error)?
            .ok_or_else(|| not_found("Unknown async batch or run."))?;
        if store.async_batch(&batch).map_err(error)?.is_none() {
            return Err(not_found("ID is not an async batch or run."));
        }
        (batch, Some(run))
    };
    let mut tasks = store.async_tasks(&batch).map_err(error)?;
    if let Some(run) = &run {
        tasks.retain(|task| &task.run_id == run);
    }
    if tasks.is_empty() {
        return Err(not_found("Unknown async batch or run."));
    }
    Ok((batch, run, tasks))
}

fn accepted(store: &Store, id: &BatchId, generic: bool) -> Result<Value, FabricError> {
    let tasks = store.async_tasks(id).map_err(error)?;
    if generic {
        let tasks = tasks
            .into_iter()
            .map(|task| {
                json!({
                    "task_id":task.task_id,"run_id":task.run_id,"status":"queued"
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"accepted":true,"batch_id":id,"tasks":tasks}))
    } else {
        let runs = tasks
            .into_iter()
            .map(|task| task.run_id)
            .collect::<Vec<_>>();
        Ok(json!({"batch_id":id,"run_ids":runs,"accepted":true}))
    }
}

// Flattening retains the v10 plan representation; old records use batch-level limits.
#[derive(Serialize, Deserialize)]
struct AcceptedPlan {
    #[serde(flatten)]
    plan: HarnessPlan,
    #[serde(default)]
    limits: Option<Options>,
}

fn request_digest(
    request: &impl Serialize,
    fake_binary: &Option<PathBuf>,
) -> Result<String, FabricError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(request, fake_binary)).map_err(error)?)
    ))
}

fn aggregate(tasks: &[AsyncTaskStatus]) -> RunState {
    if tasks.len() == 1 {
        return tasks[0].state;
    }
    if tasks.iter().all(|task| terminal(task.queue_state)) {
        if tasks.iter().all(|task| task.state == RunState::Succeeded) {
            return RunState::Succeeded;
        }
        if tasks
            .iter()
            .any(|task| task.state == RunState::NeedsAttention)
        {
            return RunState::NeedsAttention;
        }
        if tasks.iter().any(|task| task.state == RunState::Failed) {
            return RunState::Failed;
        }
        return RunState::Cancelled;
    }
    if tasks.iter().any(|task| task.state == RunState::Running) {
        RunState::Running
    } else if tasks.iter().any(|task| task.state == RunState::Finalizing) {
        RunState::Finalizing
    } else if tasks.iter().any(|task| task.state == RunState::Preparing) {
        RunState::Preparing
    } else {
        RunState::Queued
    }
}

fn recovered_report(
    root: &Path,
    store: &Store,
    run: &fabric_store::Run,
) -> Result<Option<RunReport>, FabricError> {
    let (Some(capture), Some(manifest)) = (&run.capture, &run.artifact_manifest_id) else {
        return Ok(None);
    };
    let task = store.async_task(&run.run_id).map_err(error)?;
    let plan: HarnessPlan = serde_json::from_str(&task.plan_json).map_err(error)?;
    let workspace = store
        .workspace(&run.workspace_id)
        .map_err(error)?
        .ok_or_else(|| error("Missing retained workspace metadata."))?;
    let cleanup = crate::workspace::reclaim_run(root, &run.run_id).ok();
    Ok(Some(RunReport {
        run_id: run.run_id.to_string(),
        workspace_id: run.workspace_id.to_string(),
        prepared_state_id: run.prepared_state_id.to_string(),
        base_commit_sha: run.base_commit_sha.clone(),
        prompt_digest: run.prompt_digest.clone(),
        harness: run.harness.clone(),
        harness_version: run.harness_version.clone(),
        profile: Some(plan.profile),
        state: run.state,
        exit_code: run.exit_code,
        exit_signal: run.exit_signal,
        error_kind: run.error_kind.clone(),
        created_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
        workspace_path: workspace.managed_path,
        events: capture.event_counts.clone(),
        capture: capture.clone(),
        artifact_manifest_id: Some(manifest.to_string()),
        cleanup,
        worker_attempts: store.worker_attempts(&run.run_id).map_err(error)?,
    }))
}

fn terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::NeedsAttention
    )
}
fn now() -> Result<i64, FabricError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_secs(),
    )
    .map_err(error)
}
fn invalid(message: impl Into<String>) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidArguments,
        message: message.into(),
    }
}
fn not_found(message: impl Into<String>) -> FabricError {
    FabricError {
        kind: ErrorKind::NotFound,
        message: message.into(),
    }
}
fn error(message: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: message.to_string(),
    }
}

fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T, FabricError> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).map_err(error)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(invalid("Invalid or oversized manager frame."));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).map_err(error)?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid manager protocol JSON."))
}
fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> Result<(), FabricError> {
    let bytes = serde_json::to_vec(value).map_err(error)?;
    if bytes.len() > MAX_FRAME {
        return Err(error("Manager response exceeds the bounded frame limit."));
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_protocol_rejects_oversized_input_before_allocation() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .unwrap();
        assert_eq!(
            read_frame::<Value>(&mut server).unwrap_err().kind,
            ErrorKind::InvalidArguments
        );
        let value = json!({"version":1,"root":{"path":"/private/root","device":1,"inode":2},"request":{"op":"stop"}});
        write_frame(&mut client, &value).unwrap();
        let envelope: Envelope = read_frame(&mut server).unwrap();
        assert_eq!(envelope.root.inode, 2);
        assert!(matches!(envelope.request, Request::Stop));
    }
}
