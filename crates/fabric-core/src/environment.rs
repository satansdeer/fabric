//! Trusted-local environment control. Targets choose grants; only Core owns effects.
use crate::workspace::{self, CleanupLease};
use fabric_domain::{
    EnvironmentDesired, EnvironmentId, EnvironmentRuntime, EnvironmentSelection, ErrorKind,
    FabricError, HarnessPlan, RunId,
};
use fabric_runner::target::{Action, Target, TargetPin};
use fabric_runner::{CancellationToken, CommandSpec};
use fabric_store::{EnvironmentRecord, Store};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct EnvironmentReport {
    pub environment_id: EnvironmentId,
    pub owner: String,
    pub owner_uid: u32,
    pub target: String,
    pub desired: EnvironmentDesired,
    pub runtime: EnvironmentRuntime,
    pub observed_at: i64,
    pub observation: String,
    pub capabilities: fabric_domain::EnvironmentCapabilities,
    pub home: Option<PathBuf>,
    pub runs: Vec<RunId>,
}

fn error(message: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: message.to_string(),
    }
}
fn unsupported(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::UnsupportedCapability,
        message: message.into(),
    }
}
fn now() -> Result<i64, FabricError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .ok_or_else(|| error("Invalid environment observation time"))
}
fn validate_selection(selection: &EnvironmentSelection) -> Result<(), FabricError> {
    if selection.owner.is_empty()
        || selection.owner.len() > 128
        || !selection
            .owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(FabricError {
            kind: ErrorKind::InvalidArguments,
            message: "Environment owner must contain 1–128 ASCII letters, digits, '_' or '-'"
                .into(),
        });
    }
    Ok(())
}
fn home(root: &Path, id: &EnvironmentId) -> PathBuf {
    root.join("environments").join(id.as_str()).join("tree")
}
fn marker(record: &EnvironmentRecord) -> String {
    format!(
        "fabric.environment.v1\n{}\n{}\n{}\n{:x}\n",
        record.environment_id,
        record.owner,
        record.owner_uid,
        Sha256::digest(record.target_json.as_bytes())
    )
}
fn owned(
    store: &Store,
    selection: &EnvironmentSelection,
    pin: &TargetPin,
) -> Result<EnvironmentRecord, FabricError> {
    validate_selection(selection)?;
    let record = store
        .environment(&selection.environment_id)
        .map_err(error)?
        .ok_or_else(|| error("Unknown environment; explicitly ensure it first"))?;
    if record.owner != selection.owner
        || record.owner_uid != unsafe { libc::geteuid() }
        || record.target_json != serde_json::to_string(pin).map_err(error)?
    {
        return Err(FabricError {
            kind: ErrorKind::AuthorizationRequired,
            message: "Environment owner/UID or immutable target binding does not match".into(),
        });
    }
    Ok(record)
}
fn report(
    root: &Path,
    store: &Store,
    record: EnvironmentRecord,
    pin: &TargetPin,
) -> Result<EnvironmentReport, FabricError> {
    let runs = store
        .environment_runs(&record.environment_id)
        .map_err(error)?;
    Ok(EnvironmentReport {
        home: if record.runtime == EnvironmentRuntime::Available && !remote_record(&record) {
            Some(home(root, &record.environment_id))
        } else {
            None
        },
        environment_id: record.environment_id,
        owner: record.owner,
        owner_uid: record.owner_uid,
        target: pin.placement.clone(),
        desired: record.desired,
        runtime: record.runtime,
        observed_at: record.observed_at,
        observation: record.observation,
        capabilities: record.capabilities,
        runs,
    })
}
fn observe(root: &Path, store: &Store, record: &EnvironmentRecord) -> Result<(), FabricError> {
    let fact = record
        .resource_json
        .as_deref()
        .ok_or_else(|| error("Creation interrupted before verified resource ownership"))
        .and_then(|saved| {
            let resource = workspace::environment_resource(
                root,
                &record.environment_id,
                &marker(record),
                Some(saved),
                record.destroy_intent,
            )?;
            if !record.destroy_intent {
                resource.validate_home()?;
            }
            Ok(resource)
        });
    let (runtime, observation) = match fact {
        Ok(resource) if resource.absent() => (
            EnvironmentRuntime::Absent,
            "owned_resource_absent_after_destroy_intent",
        ),
        Ok(_) if record.destroy_intent => (
            EnvironmentRuntime::Unknown,
            "destroy_pending_owned_resource_present",
        ),
        Ok(_) => (EnvironmentRuntime::Available, "owned_home_verified"),
        Err(_) => (EnvironmentRuntime::Unknown, "resource_ownership_unknown"),
    };
    store
        .observe_environment(&record.environment_id, runtime, observation, now()?)
        .map_err(error)
}

pub fn ensure(
    root: &Path,
    selection: &EnvironmentSelection,
    placement: &str,
) -> Result<EnvironmentReport, FabricError> {
    validate_selection(selection)?;
    fabric_project::initialize(root)?;
    let (root, store) = workspace::open_store(root)?;
    let pin = TargetPin::select(placement)?;
    let target = pin.open(&CancellationToken::default())?;
    target.require_environment(selection.interactive_exec_pty)?;
    let namespace = root.join("environments");
    match DirBuilder::new().mode(0o700).create(&namespace) {
        Ok(()) => File::open(&root)
            .and_then(|file| file.sync_all())
            .map_err(error)?,
        Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(failure) => return Err(error(failure)),
    }
    let _lease = workspace::environment_lock(&root, true)?;
    if target.is_remote() {
        return remote_lifecycle(&root, &store, selection, &pin, target, "ensure");
    }
    let result = target
        .drive(
            "environment.ensure",
            selection.environment_id.as_str(),
            &[Action::EnsureEnvironment],
            serde_json::json!({}),
            |_| {
                if store
                    .environment(&selection.environment_id)
                    .map_err(error)?
                    .is_none()
                {
                    let record = EnvironmentRecord {
                        environment_id: selection.environment_id.clone(),
                        owner: selection.owner.clone(),
                        owner_uid: unsafe { libc::geteuid() },
                        target_json: serde_json::to_string(&pin).map_err(error)?,
                        capabilities: target.environment_capabilities()?.clone(),
                        resource_json: None,
                        desired: EnvironmentDesired::Available,
                        runtime: EnvironmentRuntime::Unknown,
                        observed_at: now()?,
                        observation: "creation_pending".into(),
                        destroy_intent: false,
                    };
                    store.create_environment(&record).map_err(error)?;
                    // Never adopt an existing container, even if its caller-supplied ID matches.
                    let container = namespace.join(selection.environment_id.as_str());
                    DirBuilder::new()
                        .mode(0o700)
                        .create(&container)
                        .map_err(error)?;
                    DirBuilder::new()
                        .mode(0o700)
                        .create(home(&root, &selection.environment_id))
                        .map_err(error)?;
                    for relative in [".config", ".cache", ".local/share", ".local/state"] {
                        DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(home(&root, &selection.environment_id).join(relative))
                            .map_err(error)?;
                    }
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(container.join(".fabric-owner"))
                        .map_err(error)?;
                    file.write_all(marker(&record).as_bytes())
                        .and_then(|()| file.sync_all())
                        .map_err(error)?;
                    for path in [
                        home(&root, &selection.environment_id).join(".local"),
                        home(&root, &selection.environment_id),
                        container.clone(),
                        namespace.clone(),
                    ] {
                        File::open(path)
                            .and_then(|file| file.sync_all())
                            .map_err(error)?;
                    }
                    let resource = workspace::environment_resource(
                        &root,
                        &record.environment_id,
                        &marker(&record),
                        None,
                        false,
                    )?;
                    store
                        .own_environment_resource(
                            &record.environment_id,
                            &resource.authority()?,
                            now()?,
                        )
                        .map_err(error)?;
                }
                let record = owned(&store, selection, &pin)?;
                if record.desired != EnvironmentDesired::Available {
                    return Err(error(
                        "Destroyed environments are immutable tombstones; choose a new ID",
                    ));
                }
                observe(&root, &store, &record)?;
                let record = owned(&store, selection, &pin)?;
                if record.runtime != EnvironmentRuntime::Available {
                    return Err(error(
                        "Environment home ownership is unknown; no recreation or adoption",
                    ));
                }
                Ok((
                    report(&root, &store, record, &pin)?,
                    serde_json::json!({"home_verified":true}),
                ))
            },
        )?
        .into_result();
    let finished = target.finish();
    let result = result?;
    finished?;
    Ok(result)
}

/// Inspect reconciles filesystem facts, not desired state or target-reported success.
pub fn inspect(
    root: &Path,
    selection: &EnvironmentSelection,
    placement: &str,
) -> Result<EnvironmentReport, FabricError> {
    let (root, store) = workspace::open_store(root)?;
    let pin = TargetPin::select(placement)?;
    let record = owned(&store, selection, &pin)?;
    let _lease = workspace::environment_lock(&root, false)?;
    let target = pin.open(&CancellationToken::default())?;
    target.require_environment(selection.interactive_exec_pty)?;
    if target.is_remote() {
        return remote_lifecycle(&root, &store, selection, &pin, target, "inspect");
    }
    let result = target
        .drive(
            "environment.inspect",
            selection.environment_id.as_str(),
            &[Action::InspectEnvironment],
            serde_json::json!({}),
            |_| {
                observe(&root, &store, &record)?;
                let record = owned(&store, selection, &pin)?;
                let evidence =
                    serde_json::json!({"runtime":record.runtime,"observation":record.observation});
                Ok((report(&root, &store, record, &pin)?, evidence))
            },
        )?
        .into_result();
    let finished = target.finish();
    let result = result?;
    finished?;
    Ok(result)
}

pub fn destroy(
    root: &Path,
    selection: &EnvironmentSelection,
    placement: &str,
    confirmation: &str,
) -> Result<EnvironmentReport, FabricError> {
    if confirmation != selection.environment_id.as_str() {
        return Err(FabricError {
            kind: ErrorKind::AuthorizationRequired,
            message: "Explicit destroy requires --confirm equal to the exact environment ID".into(),
        });
    }
    let (root, store) = workspace::open_store(root)?;
    let pin = TargetPin::select(placement)?;
    let record = owned(&store, selection, &pin)?;
    let _lease = workspace::environment_lock(&root, true)?;
    let _runs = workspace::environment_inactive(&root, &store, &record.environment_id)?;
    let target = pin.open(&CancellationToken::default())?;
    target.require_environment(selection.interactive_exec_pty)?;
    if target.is_remote() {
        return remote_lifecycle(&root, &store, selection, &pin, target, "destroy");
    }
    let saved = record
        .resource_json
        .as_deref()
        .ok_or_else(|| error("No authenticated resource ownership; destroy denied"))?;
    let resource = workspace::environment_resource(
        &root,
        &record.environment_id,
        &marker(&record),
        Some(saved),
        record.destroy_intent,
    )?;
    if !record.destroy_intent {
        resource.validate_home()?;
    }
    let result = target
        .drive(
            "environment.destroy",
            selection.environment_id.as_str(),
            &[Action::DestroyEnvironment],
            serde_json::json!({"confirmed":true}),
            |_| {
                store
                    .intent_destroy_environment(&record.environment_id, saved, now()?)
                    .map_err(error)?;
                resource.destroy()?;
                let record = owned(&store, selection, &pin)?;
                observe(&root, &store, &record)?;
                let record = owned(&store, selection, &pin)?;
                if record.runtime != EnvironmentRuntime::Absent {
                    return Err(error("Environment destruction not observed"));
                }
                Ok((
                    report(&root, &store, record, &pin)?,
                    serde_json::json!({"owned_resource_absent":true}),
                ))
            },
        )?
        .into_result();
    let finished = target.finish();
    let result = result?;
    finished?;
    Ok(result)
}

pub fn start_stop(
    root: &Path,
    selection: &EnvironmentSelection,
    placement: &str,
    start: bool,
) -> Result<EnvironmentReport, FabricError> {
    let (_, store) = workspace::open_store(root)?;
    let pin = TargetPin::select(placement)?;
    owned(&store, selection, &pin)?;
    let target = pin.open(&CancellationToken::default())?;
    target.require_environment(selection.interactive_exec_pty)?;
    target.finish()?;
    Err(unsupported(if start {
        "Environment start is unsupported by host.local; no runtime was started"
    } else {
        "Environment stop is unsupported by host.local; no runtime was stopped"
    }))
}

pub(crate) struct HomeLease {
    _lease: CleanupLease,
    root: PathBuf,
    record: EnvironmentRecord,
}
impl HomeLease {
    pub(crate) fn validate(&self) -> Result<(), FabricError> {
        if remote_record(&self.record) {
            return Ok(());
        }
        workspace::environment_resource(
            &self.root,
            &self.record.environment_id,
            &marker(&self.record),
            self.record.resource_json.as_deref(),
            false,
        )?
        .validate_home()
    }
    pub(crate) fn configure(&self, command: &mut CommandSpec) -> Result<(), FabricError> {
        self.validate()?;
        if remote_record(&self.record) {
            return Err(unsupported("Remote HOME cannot configure a host process"));
        }
        if command.environment.iter().any(|(key, _)| {
            key == "HOME"
                || key.to_string_lossy().starts_with("XDG_")
                || key.to_string_lossy().ends_with("_HOME")
                || key.to_string_lossy().ends_with("_DIR")
        }) {
            return Err(unsupported(
                "Harness command supplies external home/config roots; environment isolation is unsupported",
            ));
        }
        let home = home(&self.root, &self.record.environment_id);
        for (key, relative) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            command
                .environment
                .push((key.into(), home.join(relative).into_os_string()));
        }
        command
            .environment
            .push(("HOME".into(), home.into_os_string()));
        Ok(())
    }
}

pub(crate) fn validate_context(plan: &HarnessPlan) -> Result<(), FabricError> {
    if !plan.profile.portable
        || plan.request.profile.as_ref().is_some_and(|profile| {
            !profile.native_config_refs.is_empty() || !profile.connection_refs.is_empty()
        })
    {
        return Err(unsupported(
            "Selected native context cannot attest a private environment HOME without credential/config export; use a credential-free portable context",
        ));
    }
    Ok(())
}

pub(crate) fn for_run(
    root: &Path,
    selection: &EnvironmentSelection,
    pin: &TargetPin,
    target: &Target,
    plan: &HarnessPlan,
) -> Result<HomeLease, FabricError> {
    target.require_environment(selection.interactive_exec_pty)?;
    if !target.is_remote() {
        validate_context(plan)?;
    }
    let (root, store) = workspace::open_store(root)?;
    let lease = workspace::environment_lock(&root, false)?;
    let record = owned(&store, selection, pin)?;
    if record.desired != EnvironmentDesired::Available || record.destroy_intent {
        return Err(error("Environment is not available for new runs"));
    }
    if target.is_remote() {
        remote_observe(&store, &record, target)?;
    } else {
        observe(&root, &store, &record)?;
    }
    let record = owned(&store, selection, pin)?;
    if record.runtime != EnvironmentRuntime::Available {
        return Err(error("Environment ownership is unknown"));
    }
    let lease = HomeLease {
        _lease: lease,
        root,
        record,
    };
    lease.validate()?;
    Ok(lease)
}

/// Live terminal ownership, deliberately distinct from run/workspace capture.
/// Field order keeps the target and its owned processes ahead of the HOME lease.
pub struct ShellSession {
    target: Target,
    home: HomeLease,
    closed: bool,
    finished: bool,
}

impl ShellSession {
    pub fn exchange(
        &mut self,
        request: &fabric_runner::pty::Exchange,
    ) -> Result<fabric_runner::pty::Reply, FabricError> {
        if self.closed {
            return Err(fabric_runner::pty::error());
        }
        self.home.validate()?;
        // Plugin errors may contain arbitrary remote strings. Never propagate
        // them into diagnostics for this content-bearing channel.
        let reply = self
            .target
            .exchange_exec(request)
            .map_err(|_| fabric_runner::pty::error())?;
        self.closed = reply.closed.is_some();
        Ok(reply)
    }

    pub fn close(&mut self, cancel: bool) -> Result<(), FabricError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let result = self
            .target
            .close_exec(cancel)
            .map_err(|_| fabric_runner::pty::error());
        self.closed = true;
        let finished = self
            .target
            .finish()
            .map_err(|_| fabric_runner::pty::error());
        result?;
        finished
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        // Also closes a normally exited stream after its final output is drained.
        if !self.finished {
            let _ = self.close(true);
        }
    }
}

pub fn shell(
    root: &Path,
    selection: &EnvironmentSelection,
    placement: &str,
    size: fabric_runner::pty::Size,
    limits: fabric_runner::pty::Limits,
    cancellation: &CancellationToken,
) -> Result<ShellSession, FabricError> {
    size.validate()?;
    limits.validate()?;
    let (root, store) = workspace::open_store(root)?;
    let pin = TargetPin::select(placement)?;
    let lease = workspace::environment_lock(&root, false)?;
    let record = owned(&store, selection, &pin)?;
    // Deny foreign identities, changed target pins, lifecycle and unsupported
    // persisted capabilities before even starting the target control process.
    if !record.capabilities.interactive_exec_pty {
        return Err(unsupported(
            "Target does not support environment PTY sessions",
        ));
    }
    if record.desired != EnvironmentDesired::Available
        || record.destroy_intent
        || record.runtime != EnvironmentRuntime::Available
    {
        return Err(error(
            "Environment is not available for interactive sessions",
        ));
    }
    let home_lease = HomeLease {
        _lease: lease,
        root,
        record,
    };
    home_lease.validate()?;
    let target = pin.open(cancellation)?;
    target.require_environment(true)?;
    if target.is_remote() {
        remote_observe(&store, &home_lease.record, &target)?;
        let opened = target.remote_call(
            "pty.open",
            serde_json::json!({
                "environment": remote_input(&home_lease.record)?,
                "size": size, "limits": limits
            }),
        )?;
        if opened != serde_json::json!({"opened":true}) {
            return Err(error("Invalid remote PTY receipt"));
        }
        return Ok(ShellSession {
            target,
            home: home_lease,
            closed: false,
            finished: false,
        });
    }
    let home = home(&home_lease.root, &home_lease.record.environment_id);
    let namespace = home
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(fabric_runner::pty::error)?;
    let request = fabric_runner::pty::OpenExec {
        version: 1,
        placement: placement.into(),
        environment_id: selection.environment_id.clone(),
        owner: selection.owner.clone(),
        owner_uid: home_lease.record.owner_uid,
        home_identity: fabric_runner::pty::Identity::of(
            &fs::symlink_metadata(&home).map_err(error)?,
        ),
        namespace_identity: fabric_runner::pty::Identity::of(
            &fs::symlink_metadata(namespace).map_err(error)?,
        ),
        home,
        size,
        limits,
    };
    home_lease.validate()?;
    let mut session = ShellSession {
        target,
        home: home_lease,
        closed: false,
        finished: false,
    };
    session
        .target
        .open_exec(&request)
        .map_err(|_| fabric_runner::pty::error())?;
    session.closed = false;
    Ok(session)
}

fn remote_record(record: &EnvironmentRecord) -> bool {
    record
        .resource_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .is_some_and(|v| v["backend"] == "remote.v1")
}

pub(crate) fn remote_input(record: &EnvironmentRecord) -> Result<serde_json::Value, FabricError> {
    let resource: serde_json::Value = serde_json::from_str(
        record
            .resource_json
            .as_deref()
            .ok_or_else(|| error("Remote resource is not bound"))?,
    )
    .map_err(error)?;
    if resource["backend"] != "remote.v1"
        || resource["resource_id"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 4096)
    {
        return Err(error("Invalid remote ownership binding"));
    }
    Ok(
        serde_json::json!({"environment_id":record.environment_id,"owner":record.owner,"resource":resource}),
    )
}

fn remote_observe(
    store: &Store,
    record: &EnvironmentRecord,
    target: &Target,
) -> Result<(), FabricError> {
    let reply = target.remote_call("environment.inspect", remote_input(record)?)?;
    let expected: serde_json::Value =
        serde_json::from_str(record.resource_json.as_deref().unwrap()).map_err(error)?;
    if reply["resource"] != expected || reply["available"] != true {
        store
            .observe_environment(
                &record.environment_id,
                EnvironmentRuntime::Unknown,
                "remote_binding_unavailable",
                now()?,
            )
            .map_err(error)?;
        return Err(error("Remote resource binding changed or unavailable"));
    }
    store
        .observe_environment(
            &record.environment_id,
            EnvironmentRuntime::Available,
            "remote_owned_resource_verified",
            now()?,
        )
        .map_err(error)
}

fn remote_lifecycle(
    root: &Path,
    store: &Store,
    selection: &EnvironmentSelection,
    pin: &TargetPin,
    target: Target,
    operation: &str,
) -> Result<EnvironmentReport, FabricError> {
    if store
        .environment(&selection.environment_id)
        .map_err(error)?
        .is_none()
    {
        if operation != "ensure" {
            return Err(error("Unknown remote environment"));
        }
        store
            .create_environment(&EnvironmentRecord {
                environment_id: selection.environment_id.clone(),
                owner: selection.owner.clone(),
                owner_uid: unsafe { libc::geteuid() },
                target_json: serde_json::to_string(pin).map_err(error)?,
                capabilities: target.environment_capabilities()?.clone(),
                resource_json: None,
                desired: EnvironmentDesired::Available,
                runtime: EnvironmentRuntime::Unknown,
                observed_at: now()?,
                observation: "remote_creation_pending".into(),
                destroy_intent: false,
            })
            .map_err(error)?;
    }
    let pending = owned(store, selection, pin)?;
    if pending.resource_json.is_none() {
        if operation != "ensure"
            || pending.destroy_intent
            || pending.desired != EnvironmentDesired::Available
        {
            return Err(error("Remote creation is incomplete"));
        }
        let reply = target.remote_call(
            "environment.ensure",
            serde_json::json!({
                "environment_id":selection.environment_id,"owner":selection.owner,"resource":null
            }),
        )?;
        let resource = &reply["resource"];
        if resource["backend"] != "remote.v1"
            || resource["resource_id"]
                .as_str()
                .is_none_or(|s| s.is_empty() || s.len() > 4096)
        {
            return Err(error("Invalid remote resource ownership evidence"));
        }
        store
            .own_environment_resource(
                &selection.environment_id,
                &serde_json::to_string(resource).map_err(error)?,
                now()?,
            )
            .map_err(error)?;
    }
    let record = owned(store, selection, pin)?;
    if operation == "destroy" {
        let input = remote_input(&record)?;
        let saved = record.resource_json.as_deref().unwrap();
        store
            .intent_destroy_environment(&record.environment_id, saved, now()?)
            .map_err(error)?;
        let reply = target.remote_call("environment.destroy", input)?;
        if reply["absent"] != true {
            return Err(error("Remote destruction not observed"));
        }
        store
            .observe_environment(
                &record.environment_id,
                EnvironmentRuntime::Absent,
                "remote_owned_resource_absent",
                now()?,
            )
            .map_err(error)?;
    } else {
        if record.desired != EnvironmentDesired::Available || record.destroy_intent {
            return Err(error("Destroyed environments are immutable tombstones"));
        }
        remote_observe(store, &record, &target)?;
    }
    target.finish()?;
    report(root, store, owned(store, selection, pin)?, pin)
}
