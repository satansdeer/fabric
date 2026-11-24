use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
#[path = "../../fabric-runner/tests/support/local_target.rs"]
mod local_target;
#[path = "../../fabric-runner/tests/support/reference_plugins.rs"]
mod reference_plugins;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-run-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        reference_plugins::install(&path);
        local_target::install(&path, Path::new(env!("CARGO_BIN_EXE_fabric-local-target")));
        Self(path)
    }
    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.0)
            .env(
                "FABRIC_PLUGIN_REGISTRY",
                self.0.canonicalize().unwrap().join("plugin-registry"),
            )
            .env("UNSELECTED_SECRET", "must-not-leak")
            .arg("--root")
            .arg(self.0.join("storage"));
        command
    }
    fn options(&self) -> fabric_core::run::RunOptions {
        let registry = self.0.canonicalize().unwrap().join("plugin-registry");
        let manifest = fabric_runner::plugin::PluginRegistry::load(&registry)
            .unwrap()
            .select(
                &"local".parse().unwrap(),
                fabric_runner::plugin::PluginKind::Target,
            )
            .unwrap();
        let mut pin = fabric_runner::target::TargetPin::from_manifest(manifest, "local").unwrap();
        pin.registry = Some(registry);
        fabric_core::run::RunOptions {
            target_pin: Some(pin),
            ..Default::default()
        }
    }
    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        success(&out);
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn setup(&self) -> (PathBuf, serde_json::Value) {
        let repo = self.0.join("repo ; literal");
        fs::create_dir(&repo).unwrap();
        self.git(&repo, &["init", "--quiet"]);
        fs::write(repo.join("tracked"), "original").unwrap();
        fs::write(repo.join(".gitignore"), ".deps/\n").unwrap();
        self.git(&repo, &["add", "."]);
        self.git(&repo, &["commit", "--quiet", "-m", "initial"]);
        success(
            &self
                .cli()
                .args(["project", "add", "--name", "test", "--path"])
                .arg(&repo)
                .output()
                .unwrap(),
        );
        (repo, self.prepare())
    }
    fn prepare(&self) -> serde_json::Value {
        let recipe = self.0.join("recipe.toml");
        fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n").unwrap();
        json(
            &self
                .cli()
                .args(["project", "prepare", "test", "--json", "--recipe"])
                .arg(recipe)
                .output()
                .unwrap(),
        )
    }
    fn run(&self, exit: i32) -> Output {
        let prompt = self.0.join("prompt ; $(touch INJECTED).json");
        fs::write(&prompt, serde_json::to_vec(&serde_json::json!({
            "required_files": [".deps/ready"],
            "writes": [{"path":"tracked", "content":"worker tracked"}, {"path":"new source ; literal", "content":"worker untracked"}],
            "events": [{"type":"fixture", "value":42, "payload":"x".repeat(128 * 1024)}], "exit_code":exit,
            "absent_environment": ["UNSELECTED_SECRET"],
        })).unwrap()).unwrap();
        self.cli()
            .args(["run", "test", "--harness", "fake", "--prompt-file"])
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap()
    }
    fn script_fake(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let executable = self.0.join("fake-script");
        fs::write(&executable, format!(
            "#!/bin/sh\nset -eu\numask 077\nif [ \"$1\" = --version ]; then printf 'fabric-fake-harness 1\\n'; exit 0; fi\n{body}\n"
        )).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }
    fn target(&self, id: &str, behavior: &str) {
        use std::os::unix::fs::PermissionsExt;
        let executable = self.0.canonicalize().unwrap().join(format!("target-{id}"));
        fs::write(&executable, format!(r#"#!/usr/bin/python3
import json, os, sys, time
identity = {id:?}
started = False
grants = {{}}
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    with open({log:?}, 'a') as log: log.write(json.dumps(request) + '\n')
    if method == 'initialize':
        result = dict(protocol=1, id=identity, version='0.1.0', kind='target')
    elif method in ('health', 'shutdown'):
        result = dict(ready=True)
    elif method == 'target.capabilities':
        result = dict(placements=[identity], operations=['prepare','start','inspect','cancel','collect','reclaim'], optional=['environment.v1'] if {behavior:?}.startswith('environment') else [])
    elif method == 'target.environment.capabilities':
        assert request['params'] == dict(version=1)
        result = dict(version=1, persistent_home=True, interactive_exec_pty=False, infrastructure_diagnostics=False, operations=['ensure','inspect','destroy','workspace'])
        if {behavior:?} == 'environment-unknown': result['invented_authority'] = True
    elif method == 'target.placement':
        result = dict(backend='unsupported' if {behavior:?} == 'unsupported' else 'host.local')
    elif method == 'target.completed':
        grant = request['params']['grant']
        op = grants[grant]
        behavior = {behavior:?}
        marker = {failure_marker:?}
        if (behavior.startswith('receipt-' + op + '-') or (behavior == 'recovery-collect' and op == 'collect')) and not os.path.exists(marker):
            open(marker, 'w').close()
            mode = behavior.rsplit('-', 1)[1]
            if mode == 'disconnect': os._exit(0)
            result = dict(received='wrong') if mode == 'wrong' else dict(ready=True)
        else:
            result = dict(received=grant)
    else:
        op = method[len('target.'):]
        if op == 'start': started = True
        if op == 'start' and {behavior:?} in ('source-witness', 'source-mismatch', 'environment'):
            import sqlite3
            db = sqlite3.connect({database:?})
            row = db.execute('SELECT receipt_json, r.state, r.started_at, a.started_at FROM run_launch_receipts l JOIN runs r USING(run_id) JOIN worker_attempts a USING(run_id) WHERE r.run_id=?', (request['params']['run_id'],)).fetchone()
            assert row is not None and row[1:] == ('PREPARING', None, None), row
            with open({source_snapshot:?}, 'w') as out: json.dump(json.loads(row[0]), out)
            if {behavior:?} == 'source-mismatch':
                with open(os.path.join(json.loads(row[0])['workspace_path'], 'tracked'), 'w') as out: out.write('tampered')
            db.close()
        if started and op in ('inspect', 'cancel') and os.path.exists({await_worker:?}):
            deadline = time.monotonic() + .15
            while not os.path.exists({worker_ready:?}) and time.monotonic() < deadline: time.sleep(.001)
        if started and op == 'cancel' and {behavior:?} == 'cancel-crash': os._exit(0)
        if started and op == 'inspect' and {behavior:?} == 'recovery-collect': os._exit(0)
        if started and op == 'inspect' and {behavior:?} in ('crash', 'environment-crash'): os._exit(0)
        if started and op == 'inspect' and {behavior:?} == 'hang': __import__('time').sleep(10)
        job = request['params']
        grants[job['grant']] = op
        action = job['actions'][-1] if op == 'prepare' and {behavior:?} == 'copy' else job['actions'][0]
        result = dict(grant=job['grant'], action=action)
        if started and op == 'inspect' and {behavior:?} == 'forge':
            result['exit_code'] = 0
    print(json.dumps(dict(jsonrpc='2.0', id=request['id'], result=result)), flush=True)
    if method == 'shutdown': break
"#, log=self.0.join(format!("target-{id}.jsonl")).to_str().unwrap(),
            database=self.0.join("storage/fabric.db").to_str().unwrap(),
            source_snapshot=self.0.join("prestart-source.json").to_str().unwrap(),
            failure_marker=self.0.join("receipt-failed").to_str().unwrap(),
            await_worker=self.0.join("await-worker").to_str().unwrap(),
            worker_ready=self.0.join("worker-ready").to_str().unwrap())).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = self.0.join("target.json");
        fs::write(
            &manifest,
            serde_json::to_vec(&serde_json::json!({
                "id":id,"version":"0.1.0","kind":"target","protocol":{"min":1,"max":1},
                "executable":executable,"args":[],"capabilities":["target.execute"]
            }))
            .unwrap(),
        )
        .unwrap();
        success(
            &self
                .cli()
                .args(["plugins", "--registry"])
                .arg(self.0.canonicalize().unwrap().join("plugin-registry"))
                .arg(if id == "local" { "upgrade" } else { "install" })
                .arg(manifest)
                .output()
                .unwrap(),
        );
    }
    fn native_profile(&self) -> serde_json::Value {
        use std::os::unix::fs::PermissionsExt;
        let home = self.0.join("selected-home");
        let agent = home.join(".omp/agent");
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            agent.join("config.yml"),
            "theme: dark\napiKey: config-secret-sentinel\n",
        )
        .unwrap();
        fs::write(agent.join("agent.db"), "opaque-auth-secret-sentinel").unwrap();
        let binary = self.0.join("omp");
        fs::write(&binary, r#"#!/bin/sh
set -eu
test -n "$HOME"
test -d "$HOME"
test -n "$XDG_CONFIG_HOME"
test -n "$XDG_DATA_HOME"
test -n "$XDG_STATE_HOME"
test -n "$XDG_CACHE_HOME"
test -z "${UNSELECTED_SECRET-}${OMP_PROFILE-}${PI_PROFILE-}${PI_CONFIG_FILES-}${CLAUDE_CONFIG_DIR-}${HTTPS_PROXY-}"
if [ "$1" = --version ]; then
    test ! -e "$HOME/.omp"
    test -z "${OPENAI_API_KEY-}"
    printf 'omp/19.1.0\n'
    exit 0
fi
test -f "$HOME/.omp/agent/config.yml"
test -f "$HOME/.omp/agent/agent.db"
if [ -n "${OPENAI_API_KEY-}" ]; then
    test "$OPENAI_API_KEY" = authorized-key-secret-sentinel
fi
printf 'native fixture changed source' > tracked
printf '{"type":"message_end"}\n'
"#).unwrap();
        fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).unwrap();
        serde_json::json!({
            "harness_id":"omp", "harness_version":"omp/19.1.0",
            "native_config_refs":[{"kind":"native-home","path":home}],
            "connection_refs":[{"kind":"native-auth","name":"native-home"}]
        })
    }
    fn native_cli(&self) -> Command {
        let mut command = self.cli();
        command
            .env("PATH", format!("{}:/usr/bin:/bin", self.0.display()))
            .env("OPENAI_API_KEY", "authorized-key-secret-sentinel")
            .env("XDG_CONFIG_HOME", self.0.join("hostile-config"))
            .env("XDG_DATA_HOME", self.0.join("hostile-data"))
            .env("XDG_STATE_HOME", self.0.join("hostile-state"))
            .env("XDG_CACHE_HOME", self.0.join("hostile-cache"))
            .env("OMP_PROFILE", "hostile-profile")
            .env("PI_PROFILE", "hostile-profile")
            .env("PI_CONFIG_FILES", "hostile-files")
            .env("CLAUDE_CONFIG_DIR", "hostile-foreign")
            .env("HTTPS_PROXY", "hostile-proxy");
        command
    }
    fn inventory(&self) -> Vec<i64> {
        let db = rusqlite::Connection::open_with_flags(
            self.0.join("storage/fabric.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "runs",
            "workspaces",
            "batches",
            "batch_tasks",
            "worker_attempts",
        ]
        .map(|table| {
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .to_vec()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            use std::os::unix::fs::PermissionsExt;
            if fs::symlink_metadata(path).unwrap().is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                for entry in fs::read_dir(path).unwrap() {
                    writable(&entry.unwrap().path());
                }
            }
        }
        writable(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn success(out: &Output) {
    assert!(out.status.success(), "{out:?}");
}
fn json(out: &Output) -> serde_json::Value {
    success(out);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn actual_fork_source_receipt_precedes_start_and_launch_fails_closed() {
    for fault in ["none", "source-mismatch", "receipt-persistence"] {
        let fixture = Fixture::new();
        let (_, prepared) = fixture.setup();
        fixture.target(
            "local",
            if fault == "source-mismatch" {
                fault
            } else {
                "source-witness"
            },
        );
        let database = fixture.0.join("storage/fabric.db");
        let db = rusqlite::Connection::open(&database).unwrap();
        if fault == "receipt-persistence" {
            db.execute_batch(
                "CREATE TRIGGER injected_receipt_failure BEFORE INSERT ON run_launch_receipts
                 BEGIN SELECT RAISE(ABORT, 'injected receipt persistence failure'); END;",
            )
            .unwrap();
        }
        let observed = fixture.0.join("worker-source.json");
        // First harness action reads the manager's receipt. The target snapshot proves
        // the identical durable row existed while both run and attempt were unstarted.
        let binary = fixture.script_fake(&format!(
            r#"exec /usr/bin/python3 - <<'PY'
import json, os, sqlite3
db = sqlite3.connect({database:?})
receipt = json.loads(db.execute('SELECT receipt_json FROM run_launch_receipts').fetchone()[0])
with open({snapshot:?}) as f: assert receipt == json.load(f)
assert os.path.samefile(receipt['workspace_path'], os.getcwd())
st = os.stat('.')
assert (st.st_dev, st.st_ino) == (receipt['workspace_device'], receipt['workspace_inode'])
assert receipt['source_commit_sha'] == {commit:?}
assert receipt['prepared_state_id'] == {state:?}
assert receipt['actual_sha256'] == receipt['source']['prepared_sha256']
assert receipt['source']['checkout_sha256'] != receipt['source']['prepared_sha256']
with open('tracked') as f: assert f.read() == 'original'
with open('.deps/ready') as f: assert f.read() == 'ready'
with open({observed:?}, 'w') as f: json.dump(receipt, f)
PY"#,
            database = database.to_str().unwrap(),
            snapshot = fixture.0.join("prestart-source.json").to_str().unwrap(),
            commit = prepared["source_commit_sha"].as_str().unwrap(),
            state = prepared["state_id"].as_str().unwrap(),
            observed = observed.to_str().unwrap(),
        ));
        let prompt = fixture.0.join("prompt.json");
        fs::write(&prompt, "{}").unwrap();
        let output = fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(binary)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap();
        if fault == "none" {
            let report = json(&output);
            let receipt: serde_json::Value =
                serde_json::from_slice(&fs::read(&observed).unwrap()).unwrap();
            let id = report["run_id"].as_str().unwrap();
            let retained = json(
                &fixture
                    .cli()
                    .args(["runs", "show", id, "--json"])
                    .output()
                    .unwrap(),
            );
            assert_eq!(retained["launch_receipt"], receipt);
            assert_eq!(retained["workspace"]["state"], "RECLAIMED");
            assert!(!Path::new(receipt["workspace_path"].as_str().unwrap()).exists());
            let artifacts = json(
                &fixture
                    .cli()
                    .args(["runs", "artifacts", id, "--json"])
                    .output()
                    .unwrap(),
            );
            assert_eq!(artifacts["launch_receipt"], receipt);
        } else {
            assert!(!output.status.success(), "{output:?}");
            assert!(!observed.exists(), "{fault} started the harness");
            let (run_started, attempt_started): (Option<i64>, Option<i64>) = db.query_row(
                "SELECT r.started_at, a.started_at FROM runs r JOIN worker_attempts a USING(run_id)",
                [], |r| Ok((r.get(0)?, r.get(1)?)),
            ).unwrap();
            assert_eq!((run_started, attempt_started), (None, None));
            let path: String = db
                .query_row("SELECT managed_path FROM workspaces", [], |r| r.get(0))
                .unwrap();
            assert!(Path::new(&path).is_dir(), "actual fork must be retained");
            assert_ne!(path, prepared["prepared_path"].as_str().unwrap());
            if fault == "source-mismatch" {
                assert_eq!(
                    fs::read(Path::new(&path).join("tracked")).unwrap(),
                    b"tampered"
                );
                assert_eq!(
                    fs::read(
                        Path::new(prepared["prepared_path"].as_str().unwrap()).join("tracked")
                    )
                    .unwrap(),
                    b"original"
                );
                assert!(fixture.0.join("prestart-source.json").exists());
            } else {
                let count: i64 = db
                    .query_row("SELECT count(*) FROM run_launch_receipts", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(count, 0);
                assert!(!fixture.0.join("prestart-source.json").exists());
            }
        }
    }
}

#[test]
fn installed_remote_refuses_placement_without_starting_a_local_worker() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("fake-remote", "unsupported");
    let marker = fixture.0.join("LOCAL_FALLBACK");
    let worker = fixture.script_fake(&format!("touch '{}'; exit 0", marker.display()));
    let prompt = fixture.0.join("remote-prompt");
    fs::write(&prompt, "never execute").unwrap();
    let before = fixture.inventory();
    let output = fixture
        .cli()
        .args([
            "run",
            "test",
            "--harness",
            "fake",
            "--placement",
            "fake-remote",
            "--fake-binary",
        ])
        .arg(worker)
        .arg("--prompt-file")
        .arg(prompt)
        .args(["--copy-limit-bytes", "10485760", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "unsupported_placement");
    assert_eq!(fixture.inventory(), before);
    assert!(!fixture.0.join("LOCAL_FALLBACK").exists());
}

#[test]
fn installed_nonlocal_id_selects_copy_and_drives_real_worker_artifacts_and_reclaim() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("alternate", "copy");
    let prompt = fixture.0.join("alternate-prompt");
    fs::write(
        &prompt,
        r#"{"writes":[{"path":"tracked","content":"alternate target"}]}"#,
    )
    .unwrap();
    let report = json(
        &fixture
            .cli()
            .args([
                "run",
                "test",
                "--harness",
                "fake",
                "--placement",
                "alternate",
                "--copy-limit-bytes",
                "10485760",
                "--json",
                "--prompt-file",
            ])
            .arg(prompt)
            .output()
            .unwrap(),
    );
    assert_eq!(report["state"], "SUCCEEDED");
    assert_eq!(report["cleanup"]["action"], "reclaimed");
    assert!(!Path::new(report["workspace_path"].as_str().unwrap()).exists());
    verified_manifest(&fixture, &report);
    let restored = restored_workspace(&fixture, &report);
    assert_eq!(
        fs::read_to_string(restored.join("tracked")).unwrap(),
        "alternate target"
    );
    let calls: Vec<serde_json::Value> =
        fs::read_to_string(fixture.0.join("target-alternate.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let prepared = calls
        .iter()
        .find(|call| {
            call["method"] == "target.completed"
                && call["params"]["evidence"].get("copied_files").is_some()
        })
        .unwrap();
    assert_eq!(prepared["params"]["evidence"]["cloned_files"], 0);
    assert!(
        prepared["params"]["evidence"]["copied_files"]
            .as_u64()
            .unwrap()
            > 0
    );
    for (method, evidence) in [
        ("start", "exit_code"),
        ("inspect", "exited"),
        ("cancel", "termination_requested"),
        ("collect", "artifact_count"),
        ("reclaim", "removed"),
    ] {
        assert!(
            calls
                .iter()
                .any(|call| call["method"] == format!("target.{method}"))
        );
        assert!(calls.iter().any(|call| call["method"] == "target.completed"
            && call["params"]["evidence"].get(evidence).is_some()));
    }
}

#[test]
fn target_failure_after_spawn_preserves_terminal_capture_and_safe_recovery() {
    for behavior in ["crash", "forge", "hang", "cancel-crash"] {
        let fixture = Fixture::new();
        fixture.setup();
        fixture.target("local", behavior);
        fs::write(fixture.0.join("await-worker"), "").unwrap();
        let binary = fixture.script_fake(&format!(
            "printf '{{\"type\":\"text\",\"text\":\"host observed\"}}\\n'\nprintf 'host stderr\\n' >&2\nprintf '%s' $$ > parent.pid\n/bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' &\nprintf '%s' $! > child.pid\ntouch '{}'\nwhile :; do sleep 1; done",
            fixture.0.join("worker-ready").display()
        ));
        let prompt = fixture.0.join("prompt.json");
        fs::write(&prompt, "{}").unwrap();
        let output = fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(binary)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--timeout-seconds", "1", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{behavior}: {output:?}");
        let runs = json(
            &fixture
                .cli()
                .args(["runs", "list", "--json"])
                .output()
                .unwrap(),
        );
        let id = runs[0]["run_id"].as_str().unwrap();
        let before = json(
            &fixture
                .cli()
                .args(["runs", "show", id, "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(before["run"]["state"], "NEEDS_ATTENTION");
        assert_eq!(before["run"]["error_kind"], "target_protocol");
        assert!(before["run"]["exit_signal"].is_number());
        assert!(
            before["run"]["capture"]["stdout_bytes_seen"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            before["run"]["capture"]["stderr_bytes_seen"]
                .as_u64()
                .unwrap()
                > 0
        );
        let tree = Path::new(before["workspace"]["managed_path"].as_str().unwrap());
        for name in ["parent.pid", "child.pid"] {
            let pid = fs::read_to_string(tree.join(name)).unwrap();
            let state = Command::new("/bin/ps")
                .args(["-p", &pid, "-o", "stat="])
                .output()
                .unwrap();
            let state = String::from_utf8(state.stdout).unwrap();
            assert!(
                state.trim().is_empty() || state.trim().starts_with('Z'),
                "{pid} survived: {state}"
            );
        }
        let result = json(
            &fixture
                .cli()
                .args(["recover", id, "--apply", "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(result["action"], "reclaimed", "{result}");
        let after = json(
            &fixture
                .cli()
                .args(["runs", "show", id, "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(after["run"]["state"], "NEEDS_ATTENTION");
        for key in ["exit_code", "exit_signal", "capture", "error_kind"] {
            assert_eq!(after["run"][key], before["run"][key]);
        }
        assert!(after["run"]["artifact_manifest_id"].is_string());
        assert!(!tree.exists());
        assert_sealed_artifacts(&fixture, id);
    }
}

#[test]
fn target_failure_survives_capture_persistence_failure() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("local", "crash");
    fs::write(fixture.0.join("await-worker"), "").unwrap();
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER injected_capture_failure BEFORE UPDATE OF capture_metadata ON runs
         WHEN NEW.capture_metadata IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected capture persistence failure'); END;",
    )
    .unwrap();
    let binary = fixture.script_fake(&format!(
        "printf 'retained output\\n'\ntouch '{}'\nwhile :; do sleep 1; done",
        fixture.0.join("worker-ready").display()
    ));
    let prompt = fixture.0.join("prompt.json");
    fs::write(&prompt, "{}").unwrap();
    let output = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(binary)
        .arg("--prompt-file")
        .arg(prompt)
        .args(["--timeout-seconds", "1", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let (id, state, error, signal, capture): (String, String, String, i64, Option<String>) = db
        .query_row(
            "SELECT run_id, state, error_kind, exit_signal, capture_metadata FROM runs",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(state, "NEEDS_ATTENTION");
    assert_eq!(error, "target_protocol");
    assert!(signal > 0);
    assert!(capture.is_none());
    db.execute_batch("DROP TRIGGER injected_capture_failure;")
        .unwrap();
    let recovered = json(
        &fixture
            .cli()
            .args(["recover", &id, "--apply", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(recovered["action"], "retained", "{recovered}");
    assert_eq!(recovered["reason"], "capture_metadata_unavailable");
    let after = json(
        &fixture
            .cli()
            .args(["runs", "show", &id, "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(after["run"]["error_kind"], "target_protocol");
    assert_eq!(after["run"]["exit_signal"], signal);
    assert!(after["run"]["capture"].is_null());
    assert!(Path::new(after["workspace"]["managed_path"].as_str().unwrap()).exists());
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    assert!(store.artifacts(&id.parse().unwrap()).unwrap().is_empty());
}

#[test]
fn target_failure_classification_excludes_start_accounting_failure() {
    for (fault, behavior, expected) in [
        ("start", "healthy", "artifact_capture"),
        ("start", "receipt-start-wrong", "target_protocol"),
        ("observation", "healthy", "timeout"),
        ("observation", "receipt-start-wrong", "target_protocol"),
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        fixture.target("local", behavior);
        let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
        db.execute_batch(match fault {
            "start" => {
                "CREATE TRIGGER injected_start_failure BEFORE UPDATE ON worker_attempts
                 WHEN NEW.started_at IS NOT NULL AND NEW.finished_at IS NULL
                 BEGIN SELECT RAISE(ABORT, 'injected start accounting failure'); END;"
            }
            _ => {
                "CREATE TRIGGER injected_observation_failure BEFORE INSERT ON run_observations
                 WHEN EXISTS(SELECT 1 FROM worker_attempts WHERE started_at IS NOT NULL)
                 BEGIN SELECT RAISE(ABORT, 'injected observation failure'); END;"
            }
        })
        .unwrap();
        let binary = fixture.script_fake("while :; do sleep 1; done");
        let prompt = fixture.0.join("prompt.json");
        fs::write(&prompt, "{}").unwrap();
        let output = fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(binary)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--timeout-seconds", "1", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let (state, error, signal, capture): (String, String, i64, String) = db
            .query_row(
                "SELECT state, error_kind, exit_signal, capture_metadata FROM runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, "NEEDS_ATTENTION");
        assert_eq!(error, expected);
        assert!(signal > 0);
        assert!(
            serde_json::from_str::<serde_json::Value>(&capture)
                .unwrap()
                .is_object()
        );
    }
}

fn assert_sealed_artifacts(fixture: &Fixture, id: &str) {
    use sha2::{Digest, Sha256};
    let root = fixture.0.join("storage");
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    let artifacts = store.artifacts(&id.parse().unwrap()).unwrap();
    assert!(artifacts.iter().any(|file| file.kind == "manifest"));
    for file in artifacts {
        let bytes = fs::read(root.join(file.relative_path)).unwrap();
        assert_eq!(bytes.len() as u64, file.size_bytes);
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), file.sha256);
    }
}

#[test]
fn target_failure_after_committed_effect_never_loses_host_evidence() {
    for (operation, mode) in [
        ("prepare", "disconnect"),
        ("start", "wrong"),
        ("inspect", "malformed"),
        ("cancel", "disconnect"),
        ("collect", "malformed"),
        ("reclaim", "wrong"),
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        fixture.target("local", &format!("receipt-{operation}-{mode}"));
        let output = fixture.run(0);
        assert!(!output.status.success(), "{operation}: {output:?}");
        let runs = json(
            &fixture
                .cli()
                .args(["runs", "list", "--json"])
                .output()
                .unwrap(),
        );
        let id = runs[0]["run_id"].as_str().unwrap();
        let before = json(
            &fixture
                .cli()
                .args(["runs", "show", id, "--json"])
                .output()
                .unwrap(),
        );
        assert_ne!(before["run"]["state"], "SUCCEEDED", "{operation}: {before}");
        assert_eq!(
            before["run"]["error_kind"], "target_protocol",
            "{operation}: {before}"
        );
        let tree = Path::new(before["workspace"]["managed_path"].as_str().unwrap());
        if operation == "prepare" {
            assert!(tree.parent().unwrap().join(".fabric-owner").is_file());
            assert!(before["run"]["capture"].is_null());
            assert!(before["run"]["started_at"].is_null());
            assert!(before["workspace"]["usage_bytes"].as_u64().unwrap() > 0);
            continue;
        }
        assert!(
            before["run"]["capture"].is_object(),
            "{operation}: {before}"
        );
        assert!(before["run"]["exit_code"].is_number() || before["run"]["exit_signal"].is_number());
        if operation == "start" || operation == "collect" || operation == "reclaim" {
            assert_eq!(before["run"]["exit_code"], 0);
        }
        if operation == "reclaim" {
            assert_eq!(before["workspace"]["state"], "RECLAIMED");
            assert!(!tree.exists());
            let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
            for change in [
                "state='SUCCEEDED'",
                "capture_metadata=NULL",
                "exit_code=99",
                "artifact_manifest_id=NULL",
                "workspace_id='forged'",
            ] {
                assert!(
                    db.execute(&format!("UPDATE runs SET {change} WHERE run_id=?1"), [id])
                        .is_err()
                );
            }
        } else {
            assert!(tree.exists());
            let recovered = json(
                &fixture
                    .cli()
                    .args(["recover", id, "--apply", "--json"])
                    .output()
                    .unwrap(),
            );
            assert_eq!(recovered["action"], "reclaimed", "{operation}: {recovered}");
            assert!(!tree.exists());
        }
        assert_sealed_artifacts(&fixture, id);
    }
}

#[test]
fn target_failure_during_recovery_receipt_keeps_committed_artifacts() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("local", "recovery-collect");
    assert!(!fixture.run(0).status.success());
    let runs = json(
        &fixture
            .cli()
            .args(["runs", "list", "--json"])
            .output()
            .unwrap(),
    );
    let id = runs[0]["run_id"].as_str().unwrap();
    let first = json(
        &fixture
            .cli()
            .args(["recover", id, "--apply", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(first["action"], "retained", "{first}");
    assert!(first["artifact_manifest_id"].is_string(), "{first}");
    let before = json(
        &fixture
            .cli()
            .args(["runs", "show", id, "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(before["workspace"]["state"], "CLEANUP_PENDING");
    let second = json(
        &fixture
            .cli()
            .args(["recover", id, "--apply", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(second["action"], "reclaimed", "{second}");
    assert_eq!(
        second["artifact_manifest_id"],
        first["artifact_manifest_id"]
    );
    assert_sealed_artifacts(&fixture, id);
}

#[test]
fn local_target_rejects_invalid_and_replayed_grants_before_a_second_plan() {
    use std::io::Write;
    use std::process::Stdio;
    for (grant, completed, repeat, expected_plans) in [
        ("", false, "", 0),
        ("bad grant", false, "", 0),
        ("bad\n", false, "", 0),
        ("valid", false, "valid", 1),
        ("valid", true, "valid", 1),
        ("valid", true, "distinct", 2),
    ] {
        let mut frames = vec![
            ("initialize", serde_json::json!({"protocol":1})),
            ("health", serde_json::json!({})),
        ];
        let job = |grant: &str| {
            serde_json::json!({
                "placement":"local","run_id":"fixture","grant":grant,
                "actions":[{"primitive":"process.spawn"}],"context":{}
            })
        };
        frames.push(("target.start", job(grant)));
        if completed {
            frames.push((
                "target.completed",
                serde_json::json!({"grant":grant,"evidence":{}}),
            ));
        }
        if !repeat.is_empty() {
            frames.push(("target.start", job(repeat)));
        }
        let valid = repeat == "distinct";
        if valid {
            frames.push((
                "target.completed",
                serde_json::json!({"grant":repeat,"evidence":{}}),
            ));
            frames.push(("shutdown", serde_json::json!({})));
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_fabric-local-target"))
            .arg("--fabric-plugin-stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = Vec::new();
        for (index, (method, params)) in frames.into_iter().enumerate() {
            writeln!(
                input,
                "{}",
                serde_json::json!({"jsonrpc":"2.0","id":index+1,"method":method,"params":params})
            )
            .unwrap();
        }
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), valid);
        let plans = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|response| response["result"]["action"]["primitive"] == "process.spawn")
            .count();
        assert_eq!(plans, expected_plans);
    }
}

#[test]
fn native_profile_authorization_is_required_before_batch_allocation() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let binary = fixture.0.join("omp");
    fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'omp/19.1.0\\n'; exit 0; fi\nprintf '{\"type\":\"message_end\"}\\n'\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        fixture.0.join("native-prompt"),
        "No model invocation: fixture only",
    )
    .unwrap();
    let tasks = fixture.0.join("native-tasks.json");
    fs::write(
        &tasks,
        r#"[{"id":"native","prompt_file":"native-prompt","harness":"omp"}]"#,
    )
    .unwrap();
    let output = fixture
        .cli()
        .env("PATH", format!("{}:/usr/bin:/bin", fixture.0.display()))
        .args(["run-many", "test", "--tasks"])
        .arg(&tasks)
        .args([
            "--concurrency",
            "1",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "Unauthorized native execution succeeded: {output:?}"
    );
    let failure: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("profile")
            || failure["error"]["message"]
                .as_str()
                .unwrap()
                .contains("credential"),
        "{failure}"
    );
    let db = rusqlite::Connection::open_with_flags(
        fixture.0.join("storage/fabric.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    for table in [
        "runs",
        "workspaces",
        "batches",
        "batch_tasks",
        "worker_attempts",
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} allocated before authorization");
    }
}

#[test]
fn mixed_harness_batch_normalizes_events_and_rejects_late_unsupported_requests_atomically() {
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let prompts = fixture.0.join("mixed");
    fs::create_dir(&prompts).unwrap();
    for name in ["a", "b"] {
        fs::write(
            prompts.join(format!("{name}.json")),
            serde_json::to_vec(&serde_json::json!({
                "required_files": [".deps/ready"],
                "writes": [{"path":"tracked","content":name}],
                "absent_environment": ["UNSELECTED_SECRET"]
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let manifest = prompts.join("tasks.json");
    let tasks = serde_json::json!([
        {"id":"json","prompt_file":"a.json","harness":"fake-a","request":{"model":"literal-model"}},
        {"id":"text","prompt_file":"b.json","harness":"fake-b","request":{}}
    ]);
    fs::write(&manifest, serde_json::to_vec(&tasks).unwrap()).unwrap();
    let invoke = || {
        fixture
            .cli()
            .args(["run-many", "test", "--tasks"])
            .arg(&manifest)
            .args([
                "--concurrency",
                "2",
                "--copy-limit-bytes",
                "10485760",
                "--model",
                "default-must-not-override-explicit-request",
                "--json",
            ])
            .current_dir(&repo)
            .output()
            .unwrap()
    };
    let report = json(&invoke());
    assert_eq!(report["succeeded"], true);
    let results: Vec<_> = report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| &task["result"])
        .collect();
    assert_ne!(results[0]["workspace_id"], results[1]["workspace_id"]);
    for (index, result) in results.iter().enumerate() {
        let harness = ["fake-a", "fake-b"][index];
        assert_eq!(result["harness"], harness);
        assert_eq!(result["prepared_state_id"], base["state_id"]);
        assert_eq!(result["base_commit_sha"], base["source_commit_sha"]);
        assert_eq!(result["state"], "SUCCEEDED");
        assert_eq!(result["exit_code"], 0);
        let attempt = &result["worker_attempts"][0];
        assert_eq!(attempt["harness_requested"], harness);
        assert_eq!(attempt["harness_resolved"], harness);
        assert_eq!(attempt["backend_id"], "trusted-local");
        assert_eq!(attempt["termination_reason"], serde_json::Value::Null);
        assert_eq!(
            attempt["event_cursor"],
            result["capture"]["normalized_event_count"]
        );
        assert!(attempt["event_cursor"].as_u64().unwrap() >= 4);
        assert_eq!(result["capture"]["native_failure"], serde_json::Value::Null);
        assert_eq!(result["capture"]["truncated"], false);
        verified_manifest(&fixture, result);
        assert!(!Path::new(result["workspace_path"].as_str().unwrap()).exists());
        let restored = restored_workspace(&fixture, result);
        assert_eq!(
            fs::read_to_string(restored.join("tracked")).unwrap(),
            ["a", "b"][index]
        );
        let container = artifact_container(&fixture, result);
        let events: Vec<serde_json::Value> = fs::read_to_string(container.join("events.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| {
                let record: serde_json::Value = serde_json::from_str(line).unwrap();
                record.get("event").cloned()
            })
            .collect();
        assert_eq!(events.first().unwrap()["type"], "started");
        assert_eq!(events.last().unwrap()["type"], "completed");
        assert_eq!(events.last().unwrap()["success"], true);
        assert!(events.iter().any(|event| event["type"] == "output"));
        assert!(events.iter().any(|event| event["type"] == "tool"));
        assert_eq!(
            events.len() as u64,
            attempt["event_cursor"].as_u64().unwrap()
        );
        if index == 0 {
            assert_eq!(attempt["model_requested"], "literal-model");
            assert_eq!(attempt["model_resolved"], "literal-model");
            assert!(
                events
                    .iter()
                    .any(|event| event["type"] == "usage"
                        && event["input_tokens"].as_u64().is_some())
            );
            assert!(
                fs::read_to_string(container.join("stdout.log"))
                    .unwrap()
                    .contains("literal-model")
            );
        } else {
            assert!(attempt["model_requested"].is_null());
            assert!(attempt["model_resolved"].is_null());
            assert!(!events.iter().any(|event| event["type"] == "usage"));
            assert!(
                serde_json::from_str::<serde_json::Value>(
                    fs::read_to_string(container.join("stdout.log"))
                        .unwrap()
                        .lines()
                        .next()
                        .unwrap()
                )
                .is_err()
            );
        }
    }
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(Path::new(base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        "original"
    );
    let inventory = || {
        let db = rusqlite::Connection::open_with_flags(
            fixture.0.join("storage/fabric.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "runs",
            "workspaces",
            "batches",
            "batch_tasks",
            "worker_attempts",
        ]
        .map(|table| {
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        })
    };
    let before = inventory();
    let mut unsupported = tasks;
    for request in [
        serde_json::json!({"model":"forbidden"}),
        serde_json::json!({"resume":"prior-session"}),
    ] {
        unsupported[1]["request"] = request;
        fs::write(&manifest, serde_json::to_vec(&unsupported).unwrap()).unwrap();
        let failure = invoke();
        assert!(!failure.status.success(), "{failure:?}");
        assert!(failure.stdout.is_empty());
        let failure: serde_json::Value = serde_json::from_slice(&failure.stderr).unwrap();
        assert_eq!(failure["error"]["kind"], "unsupported_capability");
        assert_eq!(inventory(), before);
    }
    for directory in ["workspaces", "fork-staging"] {
        let path = fixture.0.join("storage").join(directory);
        assert!(!path.exists() || fs::read_dir(path).unwrap().next().is_none());
    }
}

#[test]
fn native_fatal_zero_exit_is_durably_failed_even_after_capture_truncation() {
    let fixture = Fixture::new();
    fixture.setup();
    let binary = fixture.script_fake(
        "head -c 8192 /dev/zero | tr '\\000' x\nprintf '\\n{\"type\":\"failure\",\"code\":\"fixture_fatal\",\"message\":\"native fatal\"}\\n'\nexit 0",
    );
    let prompt = fixture.0.join("fatal.json");
    fs::write(&prompt, "{}").unwrap();
    for limit in [1024, 1048576] {
        let output = fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(&binary)
            .arg("--prompt-file")
            .arg(&prompt)
            .args([
                "--json",
                "--copy-limit-bytes",
                "10485760",
                "--max-log-bytes",
            ])
            .arg(limit.to_string())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["state"], "FAILED");
        assert_eq!(report["exit_code"], 0);
        assert!(report["exit_signal"].is_null());
        assert_eq!(report["error_kind"], "native_failure");
        assert!(report["capture"]["native_failure"].is_string());
        assert_eq!(report["capture"]["truncated"], limit == 1024);
        assert_eq!(report["capture"]["normalized_event_count"], 3);
        let attempt = &report["worker_attempts"][0];
        assert_eq!(attempt["exit_code"], 0);
        assert_eq!(attempt["termination_reason"], "native_failure");
        assert_eq!(attempt["event_cursor"], 3);
        verified_manifest(&fixture, &report);
        let stored = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
            .unwrap()
            .run(&report["run_id"].as_str().unwrap().parse().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, fabric_domain::RunState::Failed);
        assert_eq!(stored.error_kind.as_deref(), Some("native_failure"));
        assert_eq!(stored.exit_code, Some(0));
        if limit > 1024 {
            let container = artifact_container(&fixture, &report);
            let records: Vec<serde_json::Value> =
                fs::read_to_string(container.join("events.jsonl"))
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            assert_eq!(records.last().unwrap()["event"]["type"], "completed");
            assert_eq!(records.last().unwrap()["event"]["success"], false);
        }
    }
}

#[test]
fn unsupported_harness_requests_fail_before_run_or_batch_allocation() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("request.json");
    fs::write(&prompt, "{}").unwrap();
    let manifest = fixture.0.join("requests.json");
    fs::write(
        &manifest,
        r#"[{"id":"first","prompt_file":"request.json"},{"id":"second","prompt_file":"request.json"}]"#,
    )
    .unwrap();
    let inventory = || {
        let db = rusqlite::Connection::open_with_flags(
            fixture.0.join("storage/fabric.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let counts: Vec<i64> = [
            "runs",
            "workspaces",
            "batches",
            "batch_tasks",
            "worker_attempts",
        ]
        .into_iter()
        .map(|table| {
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .collect();
        let mut directories = Vec::new();
        for name in ["workspaces", "fork-staging"] {
            let path = fixture.0.join("storage").join(name);
            if path.exists() {
                directories.extend(
                    fs::read_dir(path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            }
        }
        directories.sort();
        (counts, directories)
    };
    let before = inventory();
    for (flags, kind) in [
        (
            vec!["--model", "unavailable-model"],
            "unsupported_capability",
        ),
        (vec!["--resume", "native-session"], "unsupported_capability"),
        (vec!["--placement", "remote"], "unsupported_placement"),
        (vec!["--hard-read-only"], "unsupported_capability"),
        (vec!["--access", "inspect"], "unsupported_capability"),
        (
            vec!["--require-capability", "usage.report"],
            "unsupported_capability",
        ),
        (vec!["--max-cost-usd", "0.25"], "unsupported_capability"),
        (vec!["--max-cost-usd", "NaN"], "invalid_arguments"),
        (vec!["--max-cost-usd", "0"], "invalid_arguments"),
        (vec!["--placement", "unknown"], "unsupported_placement"),
        (vec!["--access", "unknown"], "invalid_arguments"),
        (
            vec!["--require-capability", "unknown.feature"],
            "invalid_arguments",
        ),
        (
            vec![
                "--require-capability",
                "events.structured",
                "--require-capability",
                "session.resume",
            ],
            "unsupported_capability",
        ),
    ] {
        for many in [false, true] {
            let mut command = fixture.cli();
            if many {
                command
                    .args(["run-many", "test", "--tasks"])
                    .arg(&manifest)
                    .args(["--concurrency", "2"]);
            } else {
                command.args(["run", "test", "--prompt-file"]).arg(&prompt);
            }
            let output = command
                .args(["--harness", "fake", "--json"])
                .args(&flags)
                .output()
                .unwrap();
            assert!(!output.status.success(), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
            let failure: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(failure["error"]["kind"], kind, "{failure}");
            assert_eq!(inventory(), before, "{flags:?}, batch={many}");
        }
    }
}

#[test]
fn direct_run_service_validates_budget_before_storage_allocation() {
    let fixture = Fixture::new();
    let root = fixture.0.join("unallocated");
    let prompt = fixture.0.join("prompt.json");
    fs::write(&prompt, "{}").unwrap();
    let manifest = fixture.0.join("tasks.json");
    fs::write(&manifest, r#"[{"id":"a","prompt_file":"prompt.json"}]"#).unwrap();
    let harness =
        fabric_runner::FakeHarness::new(PathBuf::from(env!("CARGO_BIN_EXE_fabric-fake-harness")))
            .unwrap();
    let registry = fabric_runner::HarnessRegistry::default();
    let registry_options = fabric_runner::RegistryOptions {
        fake_binary: Some(PathBuf::from(env!("CARGO_BIN_EXE_fabric-fake-harness"))),
    };
    let default_harness = "fake".parse().unwrap();
    let harnesses = fabric_core::run::BatchHarnesses {
        registry: &registry,
        options: &registry_options,
        default: Some(&default_harness),
    };
    for (legacy, requested, expected) in [
        (
            Some(f64::NAN),
            None,
            fabric_domain::ErrorKind::InvalidArguments,
        ),
        (
            None,
            Some(f64::INFINITY),
            fabric_domain::ErrorKind::InvalidArguments,
        ),
        (Some(-1.0), None, fabric_domain::ErrorKind::InvalidArguments),
        (None, Some(0.0), fabric_domain::ErrorKind::InvalidArguments),
        (
            Some(0.25),
            Some(0.5),
            fabric_domain::ErrorKind::InvalidArguments,
        ),
        (
            Some(0.25),
            Some(0.25),
            fabric_domain::ErrorKind::UnsupportedCapability,
        ),
        (
            None,
            Some(0.25),
            fabric_domain::ErrorKind::UnsupportedCapability,
        ),
    ] {
        let options = fabric_core::run::RunOptions {
            max_cost_usd: legacy,
            harness_request: fabric_domain::HarnessRequest {
                max_cost_usd: requested,
                ..Default::default()
            },
            ..Default::default()
        };
        let failure = fabric_core::run::foreground(&root, "missing", &prompt, &harness, &options)
            .err()
            .expect("budget must be rejected");
        assert_eq!(failure.kind, expected, "{failure}");
        let failure =
            fabric_core::run::run_many(&root, "missing", &manifest, 2, &harnesses, &options)
                .err()
                .expect("batch budget must be rejected");
        assert_eq!(failure.kind, expected, "{failure}");
        assert!(!root.exists());
    }
}

#[test]
fn harness_registry_unknown_id_and_neutral_core_preserve_attempt_evidence() {
    let fixture = Fixture::new();
    let unknown = fixture
        .cli()
        .args([
            "run",
            "test",
            "--harness",
            "unregistered-worker",
            "--prompt-file",
            "unused",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    let failure: serde_json::Value = serde_json::from_slice(&unknown.stderr).unwrap();
    assert_eq!(failure["error"]["kind"], "unknown_harness", "{failure}");
    assert!(!fixture.0.join("storage").exists());
    for source in [
        include_str!("../../fabric-core/src/lib.rs"),
        include_str!("../../fabric-core/src/run.rs"),
        include_str!("../../fabric-core/src/inspect.rs"),
    ] {
        for vendor_marker in ["\"omp\"", "\"OMP", ".omp/", "~/.omp"] {
            assert!(!source.contains(vendor_marker), "{vendor_marker}");
        }
    }
    fixture.setup();
    let report = json(&fixture.run(0));
    let attempts = report["worker_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    let attempt = &attempts[0];
    assert_eq!(attempt["run_id"], report["run_id"]);
    assert_eq!(attempt["harness_requested"], "fake");
    assert_eq!(attempt["harness_resolved"], "fake");
    assert_eq!(attempt["harness_version"], "fabric-fake-harness 1");
    assert_eq!(attempt["backend_id"], "trusted-local");
    assert_eq!(attempt["exit_code"], 0);
    assert!(attempt["started_at"].is_i64());
    assert!(attempt["finished_at"].is_i64());
    assert!(attempt["event_cursor"].as_u64().unwrap() > 0);
    assert!(attempt["model_requested"].is_null());
    assert!(attempt["model_resolved"].is_null());
    assert_eq!(attempt["profile_digest"], report["profile"]["digest"]);
    let shown = json(
        &fixture
            .cli()
            .args(["runs", "show", report["run_id"].as_str().unwrap(), "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(shown["worker_attempts"], report["worker_attempts"]);
    let mut legacy = report;
    legacy.as_object_mut().unwrap().remove("worker_attempts");
    let decoded: fabric_core::run::RunReport = serde_json::from_value(legacy).unwrap();
    assert!(
        serde_json::to_value(decoded).unwrap()["worker_attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

fn artifact_container(fixture: &Fixture, report: &serde_json::Value) -> PathBuf {
    fixture
        .0
        .join("storage/artifacts")
        .join(report["artifact_manifest_id"].as_str().unwrap())
}

fn restored_workspace(fixture: &Fixture, report: &serde_json::Value) -> PathBuf {
    let artifacts = artifact_container(fixture, report);
    let repo = fixture.0.join("repo ; literal");
    let restored = fixture
        .0
        .join(format!("recovered-{}", report["run_id"].as_str().unwrap()));
    fixture.git(
        &repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    let patch = artifacts.join("tracked.patch");
    if fs::metadata(&patch).unwrap().len() != 0 {
        fixture.git(&restored, &["apply", "--binary", patch.to_str().unwrap()]);
    }
    success(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(artifacts.join("untracked.tar"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    restored
}

fn pending_workspace(fixture: &Fixture) -> serde_json::Value {
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER gc_test_lease AFTER UPDATE OF state ON workspaces
        WHEN NEW.state='CLEANUP_PENDING' BEGIN
        UPDATE workspaces SET lease_expires_at=9223372036854775807
        WHERE workspace_id=NEW.workspace_id; END;",
    )
    .unwrap();
    drop(db);
    let report = json(&fixture.run(0));
    assert_eq!(report["cleanup"]["reason"], "worker_lease_valid");
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute_batch("DROP TRIGGER gc_test_lease; UPDATE workspaces SET lease_expires_at=NULL;")
        .unwrap();
    report
}

fn gc(fixture: &Fixture, mode: &str) -> serde_json::Value {
    json(
        &fixture
            .cli()
            .args(["storage", "gc", mode, "--json"])
            .output()
            .unwrap(),
    )
}

#[cfg(target_os = "macos")]
#[test]
fn reconciliation_preserves_expired_live_workers_then_recovers_crashed_owner() {
    reconcile_crashed_worker(false);
}

#[cfg(target_os = "macos")]
#[test]
fn reconciliation_protects_orphan_worker_that_closed_inherited_lease_descriptor() {
    reconcile_crashed_worker(true);
}

#[cfg(target_os = "macos")]
fn reconcile_crashed_worker(close_descriptors: bool) {
    use std::io::{BufRead, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::process::Stdio;

    let fixture = Fixture::new();
    fixture.setup();
    let ready = fixture.0.join("ready");
    let resume = fixture.0.join("resume");
    for path in [&ready, &resume] {
        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }
    let close = if close_descriptors {
        "fd=3; while [ \"$fd\" -lt 64 ]; do eval \"exec $fd>&-\"; fd=$((fd+1)); done\n"
    } else {
        ""
    };
    let fake = fixture.script_fake(&format!(
        "{close}printf unique > interrupted-source\nprintf '%s\\n%s\\n' \"$$\" \"$PWD\" > '{}'\nread -r resume < '{}'\n",
        ready.display(), resume.display()
    ));
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, b"{}").unwrap();
    let mut supervisor = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(fake)
        .arg("--prompt-file")
        .arg(prompt)
        .args(["--copy-limit-bytes", "10485760", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(fs::File::open(ready).unwrap()).lines();
    let worker: i32 = lines.next().unwrap().unwrap().parse().unwrap();
    let tree = PathBuf::from(lines.next().unwrap().unwrap());
    // Register against the observed live child before releasing its FIFO. NOTE_EXIT
    // is an actual kernel process event, not a sleep/PID-reuse assumption.
    let queue = unsafe { libc::kqueue() };
    assert!(queue >= 0);
    let queue = unsafe { fs::File::from_raw_fd(queue) };
    let mut event = libc::kevent {
        ident: worker as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    assert_eq!(
        unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        },
        0
    );
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute("UPDATE workspaces SET lease_expires_at=0", [])
        .unwrap();
    db.execute("UPDATE worker_leases SET expires_at=0", [])
        .unwrap();
    let run_id: String = db
        .query_row("SELECT run_id FROM runs", [], |r| r.get(0))
        .unwrap();
    let snapshot = || {
        db.query_row(
            "SELECT state, exit_code, artifact_manifest_id FROM runs WHERE run_id=?1",
            [&run_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i32>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .unwrap()
    };
    // Wait for the durable launch callback, not PID timing or a guessed sleep.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while snapshot().0 != "RUNNING" {
        assert!(
            std::time::Instant::now() < deadline,
            "launch callback did not commit"
        );
        std::thread::yield_now();
    }
    let before = snapshot();
    let attempt_snapshot = || {
        let shown = json(
            &fixture
                .cli()
                .args(["runs", "show", &run_id, "--json"])
                .output()
                .unwrap(),
        );
        shown["worker_attempts"].clone()
    };
    let attempts_before = attempt_snapshot();
    assert_eq!(attempts_before.as_array().unwrap().len(), 1);
    assert!(attempts_before[0]["started_at"].is_i64());
    assert!(attempts_before[0]["finished_at"].is_null());
    assert!(attempts_before[0]["exit_code"].is_null());
    assert!(attempts_before[0]["exit_signal"].is_null());
    gc(&fixture, "--apply");
    assert_eq!(snapshot(), before);
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let lease = fs::File::open(fixture.0.join("storage/leases").join(&run_id)).unwrap();
    let locked = unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if close_descriptors {
        assert_eq!(locked, 0, "fixture closed the inherited lock");
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_UN) }, 0);
    } else {
        assert_ne!(locked, 0, "orphan still owns the inherited lock");
    }
    gc(&fixture, "--apply");
    assert_eq!(
        snapshot(),
        before,
        "an orphaned live worker is still protected"
    );
    assert_eq!(
        fs::read(tree.join("interrupted-source")).unwrap(),
        b"unique"
    );
    fs::OpenOptions::new()
        .write(true)
        .open(resume)
        .unwrap()
        .write_all(b"finish\n")
        .unwrap();
    let deadline = libc::timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &deadline,
            )
        },
        1
    );
    assert_eq!(event.fflags & libc::NOTE_EXIT, libc::NOTE_EXIT);
    gc(&fixture, "--apply");
    assert_eq!(snapshot(), ("NEEDS_ATTENTION".into(), None, None));
    let workspace: (String, String) = db
        .query_row("SELECT state, retention_reason FROM workspaces", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(workspace.0, "RECOVERY_REQUIRED");
    assert!(workspace.1.contains("Interrupted"));
    assert_eq!(
        db.query_row("SELECT count(*) FROM storage_reservations", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        fs::read(tree.join("interrupted-source")).unwrap(),
        b"unique"
    );
    let stable = snapshot();
    gc(&fixture, "--apply");
    assert_eq!(snapshot(), stable);
    assert_eq!(attempt_snapshot(), attempts_before);
    assert!(tree.exists());
}

#[test]
fn gc_dry_run_is_read_only_and_live_lease_is_not_cleanup_permission() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    fixture.setup();
    let report = pending_workspace(&fixture);
    let db_path = fixture.0.join("storage/fabric.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let committed_database = fs::read(&db_path).unwrap();
    db.execute(
        "UPDATE workspaces SET lease_expires_at=9223372036854775807",
        [],
    )
    .unwrap();
    assert_eq!(fs::read(&db_path).unwrap(), committed_database);
    let wal_path = fixture.0.join("storage/fabric.db-wal");
    let committed_wal = fs::read(&wal_path).unwrap();
    assert!(committed_wal.len() > 32);
    assert!(fixture.0.join("storage/fabric.db-shm").is_file());
    let artifacts = artifact_container(&fixture, &report);
    let artifact_bytes: std::collections::BTreeMap<_, _> = fs::read_dir(&artifacts)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    assert_eq!(
        gc(&fixture, "--dry-run")["items"][0]["reason"],
        "worker_lease_valid"
    );
    assert_eq!(fs::read(&db_path).unwrap(), committed_database);
    assert_eq!(fs::read(&wal_path).unwrap(), committed_wal);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM cleanup_intents", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"worker tracked");
    assert_eq!(
        fs::read(tree.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
    for (name, bytes) in &artifact_bytes {
        assert_eq!(&fs::read(artifacts.join(name)).unwrap(), bytes);
    }
    drop(db);
    assert_eq!(
        gc(&fixture, "--apply")["items"][0]["reason"],
        "worker_lease_valid"
    );
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("UPDATE workspaces SET lease_expires_at=NULL", [])
        .unwrap();
    drop(db);
    let before = fs::read(&db_path).unwrap();
    let root_entries: std::collections::BTreeSet<_> = fs::read_dir(fixture.0.join("storage"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let first = gc(&fixture, "--dry-run");
    assert_eq!(first["items"][0]["action"], "would_reclaim");
    assert_eq!(first, gc(&fixture, "--dry-run"));
    assert_eq!(fs::read(&db_path).unwrap(), before);
    let after_entries: std::collections::BTreeSet<_> = fs::read_dir(fixture.0.join("storage"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(root_entries.is_subset(&after_entries));
    for added in after_entries.difference(&root_entries) {
        assert!(added == "fabric.db-wal" || added == "fabric.db-shm");
        let metadata = fs::symlink_metadata(fixture.0.join("storage").join(added)).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), fs::metadata(&db_path).unwrap().uid());
        assert_eq!(metadata.mode() & 0o077, 0);
        assert_eq!(metadata.nlink(), 1);
    }
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"worker tracked");
    assert_eq!(
        fs::read(tree.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
    for (name, bytes) in &artifact_bytes {
        assert_eq!(&fs::read(artifacts.join(name)).unwrap(), bytes);
    }
    assert_eq!(gc(&fixture, "--apply")["items"][0]["action"], "reclaimed");
    assert!(!tree.parent().unwrap().exists());
    assert_eq!(
        fs::read(restored_workspace(&fixture, &report).join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
}

#[test]
fn gc_refuses_unsafe_ownership_paths_manifests_and_noneligible_states() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for case in [
        "outside",
        "marker_missing",
        "marker_tamper",
        "container_link",
        "tree_link",
        "manifest_missing",
        "manifest_corrupt",
        "run_active",
        "provenance_tamper",
        "retention_reason",
        "active",
        "recovery",
        "retained",
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        let report = pending_workspace(&fixture);
        let tree = PathBuf::from(report["workspace_path"].as_str().unwrap());
        let container = tree.parent().unwrap();
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"outside must survive").unwrap();
        match case {
            "outside" => {
                let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
                db.execute(
                    "UPDATE workspaces SET managed_path=?1",
                    [outside.to_str().unwrap()],
                )
                .unwrap();
            }
            "marker_missing" => fs::remove_file(container.join(".fabric-owner")).unwrap(),
            "marker_tamper" => fs::write(container.join(".fabric-owner"), b"wrong owner").unwrap(),
            "container_link" => {
                fs::rename(container, fixture.0.join("saved-container")).unwrap();
                symlink(&outside, container).unwrap();
            }
            "tree_link" => {
                fs::rename(&tree, container.join("saved-tree")).unwrap();
                symlink(&outside, &tree).unwrap();
            }
            "manifest_missing" | "manifest_corrupt" => {
                let artifacts = artifact_container(&fixture, &report);
                fs::set_permissions(&artifacts, fs::Permissions::from_mode(0o700)).unwrap();
                if case == "manifest_missing" {
                    fs::remove_file(artifacts.join("manifest.json")).unwrap();
                } else {
                    fs::set_permissions(
                        artifacts.join("manifest.json"),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                    let mut bytes = fs::read(artifacts.join("manifest.json")).unwrap();
                    bytes[0] = b'X';
                    fs::write(artifacts.join("manifest.json"), bytes).unwrap();
                    fs::set_permissions(
                        artifacts.join("manifest.json"),
                        fs::Permissions::from_mode(0o400),
                    )
                    .unwrap();
                }
                fs::set_permissions(&artifacts, fs::Permissions::from_mode(0o500)).unwrap();
            }
            "run_active" | "provenance_tamper" | "retention_reason" => {
                let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
                let sql = match case {
                    "run_active" => "UPDATE runs SET state='RUNNING'",
                    "provenance_tamper" => "UPDATE runs SET harness_version='tampered'",
                    _ => "UPDATE workspaces SET retention_reason='keep for inspection'",
                };
                db.execute(sql, []).unwrap();
            }
            state => {
                let state = match state {
                    "active" => "ACTIVE",
                    "recovery" => "RECOVERY_REQUIRED",
                    _ => "RETAINED",
                };
                let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
                db.execute("UPDATE workspaces SET state=?1", [state])
                    .unwrap();
            }
        }
        let dry = gc(&fixture, "--dry-run");
        let applied = gc(&fixture, "--apply");
        for item in [&dry["items"][0], &applied["items"][0]] {
            assert_eq!(item["action"], "refused", "{case}: {item}");
            assert!(!item["reason"].as_str().unwrap().is_empty());
        }
        assert_eq!(
            fs::read(outside.join("sentinel")).unwrap(),
            b"outside must survive",
            "{case}"
        );
        let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        assert!(
            store
                .cleanup_intent(&report["workspace_id"].as_str().unwrap().parse().unwrap())
                .unwrap()
                .is_none(),
            "{case}"
        );
        assert!(fs::symlink_metadata(container).is_ok(), "{case}");
    }
}

#[test]
fn gc_reports_unknown_paths_and_unlinks_internal_links_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.setup();
    let report = pending_workspace(&fixture);
    let outside = fixture.0.join("outside-sentinel");
    fs::write(&outside, b"not a cleanup target").unwrap();
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    symlink(&outside, tree.join("post-capture-link")).unwrap();
    let unknown = fixture.0.join("storage/fork-staging/unknown");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("unknown-bytes"), b"unknown must survive").unwrap();
    let dry = gc(&fixture, "--dry-run");
    assert_eq!(dry["unknown_paths"][0]["reason"], "unknown_path");
    assert!(tree.join("post-capture-link").is_symlink());
    let applied = gc(&fixture, "--apply");
    assert_eq!(applied["items"][0]["action"], "reclaimed");
    assert_eq!(applied["unknown_paths"][0]["reason"], "unknown_path");
    assert_eq!(fs::read(outside).unwrap(), b"not a cleanup target");
    assert_eq!(
        fs::read(unknown.join("unknown-bytes")).unwrap(),
        b"unknown must survive"
    );
}

#[test]
fn gc_reclaims_only_captured_workspaces_and_preserves_recoverable_bytes() {
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let report = json(&fixture.run(0));
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert!(
        !tree.parent().unwrap().exists(),
        "captured fork must be reclaimed"
    );
    let manifest = verified_manifest(&fixture, &report);
    let artifacts = fixture
        .0
        .join("storage/artifacts")
        .join(manifest["manifest_id"].as_str().unwrap());
    let restored = fixture.0.join("recovered");
    fixture.git(
        &repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    fixture.git(
        &restored,
        &[
            "apply",
            "--binary",
            artifacts.join("tracked.patch").to_str().unwrap(),
        ],
    );
    success(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(artifacts.join("untracked.tar"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read(restored.join("tracked")).unwrap(),
        b"worker tracked"
    );
    assert_eq!(
        fs::read(restored.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
    assert!(!fs::read(artifacts.join("stdout.log")).unwrap().is_empty());
    assert_eq!(
        fs::read(Path::new(base["prepared_path"].as_str().unwrap()).join(".deps/ready")).unwrap(),
        b"ready"
    );
    let first = fixture
        .cli()
        .args(["storage", "gc", "--dry-run", "--json"])
        .output()
        .unwrap();
    let second = fixture
        .cli()
        .args(["storage", "gc", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(json(&first)["items"][0]["reason"], "already_reclaimed");
}

#[test]
fn wall_timeout_stops_fake_parent_and_term_ignoring_child_and_persists_reason() {
    use fabric_domain::{RunId, RunState};
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("timeout.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let out = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--prompt-file"])
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "1",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("timeout must return durable run report: {out:?}"));
    assert_eq!(report["state"], "FAILED");
    assert_eq!(report["error_kind"], "timeout");
    assert!(report["exit_code"].is_null());
    assert!(report["exit_signal"].as_i64().is_some());
    assert_eq!(
        report["worker_attempts"][0]["termination_reason"],
        "timeout"
    );
    assert_eq!(
        report["worker_attempts"][0]["exit_signal"],
        report["exit_signal"]
    );
    assert!(report["worker_attempts"][0]["exit_code"].is_null());
    assert_tree_stopped(&fixture, &report);
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let run = store
        .run(&RunId::try_from(report["run_id"].as_str().unwrap().to_owned()).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.error_kind.as_deref(), Some("timeout"));
    assert_eq!(run.exit_code, None);
    assert_eq!(
        verified_manifest(&fixture, &report)["stop_reason"],
        "timeout"
    );
    assert_eq!(
        run.exit_signal,
        report["exit_signal"].as_i64().map(|signal| signal as i32)
    );
}

fn verified_manifest(fixture: &Fixture, report: &serde_json::Value) -> serde_json::Value {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = fixture.0.join("storage");
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    let id = report["run_id"].as_str().unwrap().parse().unwrap();
    let run = store.run(&id).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&run.artifact_manifest_id).unwrap(),
        report["artifact_manifest_id"]
    );
    let files = store.artifacts(&id).unwrap();
    let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
    assert_eq!(workspace.state, fabric_domain::WorkspaceState::Reclaimed);
    assert!(workspace.lease_expires_at.is_none());
    let total: u64 = files.iter().map(|file| file.size_bytes).sum();
    assert_eq!(files.len(), 14);
    let mut manifest = None;
    for file in files {
        let path = root.join(&file.relative_path);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, file.size_bytes);
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), file.sha256);
        let metadata = fs::symlink_metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o400);
        assert_eq!(metadata.nlink(), 1);
        if file.kind == "manifest" {
            manifest = Some(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
        }
    }
    let manifest = manifest.unwrap();
    assert_eq!(manifest["run_id"], report["run_id"]);
    assert_eq!(manifest["workspace_id"], report["workspace_id"]);
    assert_eq!(manifest["base_commit_sha"], report["base_commit_sha"]);
    assert_eq!(manifest["prompt_digest"], report["prompt_digest"]);
    assert_eq!(manifest["harness_version"], report["harness_version"]);
    assert_eq!(manifest["profile"], report["profile"]);
    assert_eq!(manifest["exit_code"], report["exit_code"]);
    assert_eq!(manifest["exit_signal"], report["exit_signal"]);
    assert_eq!(manifest["capture"], report["capture"]);
    assert_eq!(manifest["source_scope"], "tracked_and_nonignored_untracked");
    assert_eq!(manifest["format_version"], 2);
    assert_eq!(manifest["recipe_digest"].as_str().unwrap().len(), 64);
    assert!(total <= manifest["artifact_limit_bytes"].as_u64().unwrap());
    manifest
}

fn assert_tree_stopped(fixture: &Fixture, report: &serde_json::Value) {
    let workspace = restored_workspace(fixture, report);
    assert!(
        !Path::new(report["workspace_path"].as_str().unwrap())
            .parent()
            .unwrap()
            .exists()
    );
    for name in ["parent.heartbeat", "child.heartbeat"] {
        let bytes = fs::read(workspace.join(name)).unwrap();
        assert!(!bytes.is_empty());
        let pid = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("pid="));
        if let Some(pid) = pid {
            let state = Command::new("/bin/ps")
                .args(["-p", pid, "-o", "stat="])
                .output()
                .unwrap();
            let state = String::from_utf8(state.stdout).unwrap();
            assert!(
                state.trim().is_empty() || state.trim().starts_with('Z'),
                "writer {pid} survived cleanup: {state}"
            );
        }
    }
}

fn tree_ready(tree: &Path) -> bool {
    ["parent.heartbeat", "child.heartbeat"].iter().all(|name| {
        let Ok(contents) = fs::read_to_string(tree.join(name)) else {
            return false;
        };
        let Some((record, _)) = contents.split_once('\n') else {
            return false;
        };
        (*name == "parent.heartbeat" && record == "leader exited")
            || record
                .strip_prefix("pid=")
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some_and(|pid| pid > 0)
    })
}

fn wait_for_tree(fixture: &Fixture) -> PathBuf {
    let start = std::time::Instant::now();
    loop {
        if let Ok(entries) = fs::read_dir(fixture.0.join("storage/fork-staging")) {
            for entry in entries {
                let tree = entry.unwrap().path().join("tree");
                if tree_ready(&tree) {
                    return tree;
                }
            }
        }
        // Preparation and the batch's completed first task precede this heartbeat.
        // Bound the wait, not loaded-runner filesystem preparation to eight seconds.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(60),
            "worker was not ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn tree_readiness_requires_complete_parent_and_child_records() {
    let fixture = Fixture::new();
    let parent = fixture.0.join("parent.heartbeat");
    let child = fixture.0.join("child.heartbeat");
    fs::write(&child, "pid=456\n.").unwrap();
    for partial in ["", "pid=", "pid=123", "pid=\n", "pid=0\n"] {
        fs::write(&parent, partial).unwrap();
        assert!(!tree_ready(&fixture.0), "accepted parent {partial:?}");
    }
    fs::write(&parent, "pid=123\n.").unwrap();
    assert!(tree_ready(&fixture.0));
    fs::write(&child, "pid=456").unwrap();
    assert!(!tree_ready(&fixture.0));
    fs::write(&child, "pid=456\n.").unwrap();
    fs::write(&parent, "leader exited").unwrap();
    assert!(!tree_ready(&fixture.0));
    fs::write(&parent, "leader exited\n").unwrap();
    assert!(tree_ready(&fixture.0));
}

#[test]
fn ctrl_c_cancels_live_worker_and_preserves_observed_exit() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("cancel.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let child = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--prompt-file"])
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "10",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_tree(&fixture);
    for _ in 0..2 {
        success(
            &Command::new("/bin/kill")
                .args(["-INT", &child.id().to_string()])
                .output()
                .unwrap(),
        );
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["state"], "CANCELLED");
    assert_eq!(report["error_kind"], "cancelled");
    assert_eq!(report["exit_signal"], 15);
    assert!(report["exit_code"].is_null());
    assert_eq!(
        verified_manifest(&fixture, &report)["stop_reason"],
        "cancelled"
    );
    assert_tree_stopped(&fixture, &report);
    let run = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
        .unwrap()
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Cancelled);
    assert_eq!(run.error_kind.as_deref(), Some("cancelled"));
    assert_eq!(run.exit_signal, Some(15));
}

#[test]
fn api_cancellation_is_idempotent_and_escalates_term_ignoring_child() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("api-cancel.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let options = fabric_core::run::RunOptions {
        copy_limit_bytes: 10485760,
        limits: fabric_runner::ExecutionLimits {
            timeout: std::time::Duration::from_secs(10),
            term_grace: std::time::Duration::from_millis(600),
        },
        ..fixture.options()
    };
    let harness =
        fabric_runner::FakeHarness::new(PathBuf::from(env!("CARGO_BIN_EXE_fabric-fake-harness")))
            .unwrap();
    let report = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            fabric_core::run::foreground(
                &fixture.0.join("storage"),
                "test",
                &prompt,
                &harness,
                &options,
            )
        });
        let tree = wait_for_tree(&fixture);
        options.cancellation.cancel();
        options.cancellation.cancel();
        let before = fs::metadata(tree.join("child.heartbeat")).unwrap().len();
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            fs::metadata(tree.join("child.heartbeat")).unwrap().len() > before,
            "fixture did not survive TERM to exercise escalation"
        );
        worker.join().unwrap().unwrap()
    });
    options.cancellation.cancel(); // Completed tokens never target a stored/reused PID.
    assert_eq!(report.state, fabric_domain::RunState::Cancelled);
    assert_eq!(report.error_kind.as_deref(), Some("cancelled"));
    assert_eq!(report.exit_signal, Some(15));
    assert_eq!(
        verified_manifest(&fixture, &serde_json::to_value(&report).unwrap())["stop_reason"],
        "cancelled"
    );
    assert_tree_stopped(&fixture, &serde_json::to_value(&report).unwrap());
}

#[test]
fn natural_leader_exit_cleans_background_descendant_and_does_not_claim_timeout() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("background.json");
    fs::write(
        &prompt,
        r#"{"process_tree":{"ignore_child_term":true,"leader_exits":true}}"#,
    )
    .unwrap();
    let out = fixture
        .cli()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(["run", "test", "--harness", "fake", "--prompt-file"])
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "2",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .spawn()
        .unwrap();
    let tree = wait_for_tree(&fixture);
    // Bound actual child cleanup, not the subsequent durable fsync/publication work.
    let start = std::time::Instant::now();
    loop {
        let before = fs::read(tree.join("child.heartbeat")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        if fs::read(tree.join("child.heartbeat")).unwrap() == before {
            break;
        }
    }
    let out = out.wait_with_output().unwrap();
    let report = json(&out);
    assert_eq!(report["state"], "SUCCEEDED");
    assert_eq!(report["exit_code"], 0);
    assert!(report["exit_signal"].is_null());
    assert!(report["error_kind"].is_null());
    assert!(verified_manifest(&fixture, &report)["stop_reason"].is_null());
    assert_tree_stopped(&fixture, &report);
}

#[test]
fn batch_ctrl_c_stops_queued_launches_and_keeps_completed_durable_outcomes() {
    let fixture = Fixture::new();
    fixture.setup();
    fs::write(fixture.0.join("done.json"), "{}").unwrap();
    fs::write(
        fixture.0.join("live.json"),
        r#"{"process_tree":{"ignore_child_term":true}}"#,
    )
    .unwrap();
    let manifest = fixture.0.join("cancel-batch.json");
    fs::write(&manifest, r#"[{"id":"done","prompt_file":"done.json"},{"id":"live","prompt_file":"live.json"},{"id":"queued","prompt_file":"live.json"},{"id":"also-queued","prompt_file":"live.json"}]"#).unwrap();
    let child = batch(&fixture, &manifest, "1")
        .args(["--timeout-seconds", "10"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_tree(&fixture);
    success(
        &Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .output()
            .unwrap(),
    );
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["tasks"][0]["result"]["state"], "SUCCEEDED");
    assert_eq!(report["tasks"][1]["result"]["state"], "CANCELLED");
    assert_eq!(report["succeeded"], false);
    assert_tree_stopped(&fixture, &report["tasks"][1]["result"]);
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let id = report["batch_id"].as_str().unwrap().parse().unwrap();
    let tasks = store.batch_tasks(&id).unwrap();
    assert_eq!(store.batch_succeeded(&id).unwrap(), Some(false));
    for (i, saved) in tasks.iter().enumerate() {
        if i < 2 {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(saved.result.as_ref().unwrap()).unwrap(),
                report["tasks"][i]["result"]
            );
        } else {
            assert!(
                saved
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("Cancelled before launch")
            );
            assert_eq!(report["tasks"][i]["error"].as_str(), saved.error.as_deref());
            assert!(
                store.run(&saved.run_id).unwrap().is_none(),
                "queued task was launched"
            );
        }
    }
}

#[test]
fn batch_api_applies_wall_timeout_per_worker_without_discarding_other_results() {
    let fixture = Fixture::new();
    fixture.setup();
    fs::write(
        fixture.0.join("slow.json"),
        r#"{"process_tree":{"ignore_child_term":true}}"#,
    )
    .unwrap();
    fs::write(fixture.0.join("done.json"), "{}").unwrap();
    let options = fabric_core::run::RunOptions {
        copy_limit_bytes: 10485760,
        limits: fabric_runner::ExecutionLimits {
            timeout: std::time::Duration::from_millis(400),
            ..fabric_runner::ExecutionLimits::default()
        },
        ..fixture.options()
    };
    let registry = fabric_runner::HarnessRegistry::default();
    let registry_options = fabric_runner::RegistryOptions {
        fake_binary: Some(PathBuf::from(env!("CARGO_BIN_EXE_fabric-fake-harness"))),
    };
    let default_harness = "fake".parse().unwrap();
    let report = fabric_core::run::spawn_many(
        &fixture.0.join("storage"),
        "test",
        vec![
            fabric_core::run::TaskInput {
                id: "timeout".into(),
                prompt_file: fixture.0.join("slow.json"),
                harness: None,
                request: None,
            },
            fabric_core::run::TaskInput {
                id: "success".into(),
                prompt_file: fixture.0.join("done.json"),
                harness: None,
                request: None,
            },
        ],
        1,
        &fabric_core::run::BatchHarnesses {
            registry: &registry,
            options: &registry_options,
            default: Some(&default_harness),
        },
        &options,
    )
    .unwrap();
    assert!(!report.succeeded);
    let timed = report.tasks[0].result.as_ref().unwrap();
    assert_eq!(timed.state, fabric_domain::RunState::Failed);
    assert_eq!(timed.error_kind.as_deref(), Some("timeout"));
    assert_tree_stopped(&fixture, &serde_json::to_value(timed).unwrap());
    assert_eq!(
        report.tasks[1].result.as_ref().unwrap().state,
        fabric_domain::RunState::Succeeded
    );
}

#[test]
fn cooperative_zero_exit_after_timeout_still_persists_timeout_failure() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let executable = fixture.0.join("cooperative-fake");
    fs::write(&executable, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'fabric-fake-harness 1'; exit 0; fi\ntrap 'exit 0' TERM\nwhile :; do /bin/sleep 1; done\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let prompt = fixture.0.join("cooperative.json");
    fs::write(&prompt, "{}").unwrap();
    let out = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(executable)
        .arg("--prompt-file")
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "1",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["state"], "FAILED");
    assert_eq!(report["error_kind"], "timeout");
    assert_eq!(report["exit_code"], 0);
    assert!(report["exit_signal"].is_null());
    let run = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
        .unwrap()
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Failed);
    assert_eq!(run.error_kind.as_deref(), Some("timeout"));
    assert_eq!(run.exit_code, Some(0));
}

#[test]
fn foreground_fake_process_records_provenance_exit_and_recovers_changes_after_reclaim() {
    use fabric_domain::{RunId, RunState, WorkspaceState};
    use fabric_store::Store;
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    for exit in [0, 7] {
        let out = fixture.run(exit);
        assert_eq!(out.status.success(), exit == 0, "{out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let store = Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        let run = store
            .run(&RunId::try_from(report["run_id"].as_str().unwrap().to_owned()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            run.state,
            if exit == 0 {
                RunState::Succeeded
            } else {
                RunState::Failed
            }
        );
        assert_eq!(
            run.harness_version.as_deref(),
            Some("fabric-fake-harness 1")
        );
        assert_eq!(run.exit_code, Some(exit));
        assert_eq!(report["worker_attempts"][0]["exit_code"], exit);
        assert_eq!(
            report["worker_attempts"][0]["started_at"].as_i64(),
            run.started_at
        );
        assert_eq!(run.base_commit_sha, base["source_commit_sha"]);
        assert!(run.pid_metadata.is_some());
        assert!(run.created_at <= run.started_at.unwrap());
        assert!(run.started_at.unwrap() <= run.finished_at.unwrap());
        assert!(run.finished_at.unwrap() <= run.updated_at);
        assert_eq!(run.prompt_digest, report["prompt_digest"]);
        assert_eq!(run.prompt_digest.len(), 64);
        assert!(run.artifact_manifest_id.is_some());
        verified_manifest(&fixture, &report);
        let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
        assert_eq!(workspace.state, WorkspaceState::Reclaimed);
        assert!(workspace.retention_reason.is_none());
        assert!(
            !Path::new(&workspace.managed_path)
                .parent()
                .unwrap()
                .exists()
        );
        let tree = restored_workspace(&fixture, &report);
        assert_eq!(
            fs::read_to_string(tree.join("tracked")).unwrap(),
            "worker tracked"
        );
        assert_eq!(
            fs::read_to_string(tree.join("new source ; literal")).unwrap(),
            "worker untracked"
        );
        assert!(!tree.join("INJECTED").exists());
        let container = artifact_container(&fixture, &report);
        assert_eq!(
            fs::read(container.join("prompt.input")).unwrap(),
            fs::read(fixture.0.join("prompt ; $(touch INJECTED).json")).unwrap()
        );
        use sha2::{Digest, Sha256};
        assert_eq!(
            run.prompt_digest,
            format!(
                "{:x}",
                Sha256::digest(fs::read(container.join("prompt.input")).unwrap())
            )
        );
        let stdout = fs::read_to_string(container.join("stdout.log")).unwrap();
        assert!(stdout.len() > 64 * 1024);
        let events: Vec<serde_json::Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["type"], "started");
        assert_eq!(events[1]["value"], 42);
        assert_eq!(events.last().unwrap()["exit_code"], exit);
        assert!(fs::read(container.join("stderr.log")).unwrap().is_empty());
        assert!(
            !tree.join(".deps").exists(),
            "ignored dependencies are not source artifacts"
        );
        let patch = fixture
            .0
            .join("storage/artifacts")
            .join(run.artifact_manifest_id.as_ref().unwrap().as_str())
            .join("tracked.patch");
        assert!(
            fs::read_to_string(&patch)
                .unwrap()
                .contains("worker tracked")
        );
        assert!(
            !fs::read_to_string(&patch)
                .unwrap()
                .contains("worker untracked")
        );
        // Durable evidence remains usable after the entire original container is gone.
        assert!(
            !Path::new(&workspace.managed_path)
                .parent()
                .unwrap()
                .exists()
        );
        verified_manifest(&fixture, &report);
    }
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(Path::new(base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        "original"
    );
}

#[test]
fn run_selects_prepared_current_head_and_refuses_unprepared_head() {
    let fixture = Fixture::new();
    let (repo, original) = fixture.setup();
    fs::write(repo.join("tracked"), "second commit").unwrap();
    fixture.git(&repo, &["commit", "--quiet", "-am", "second"]);
    let out = fixture.run(0);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("prepare"));
    let current = fixture.prepare();
    assert_ne!(original["state_id"], current["state_id"]);
    fs::write(repo.join("tracked"), "dirty not imported").unwrap();
    let report = json(&fixture.run(0));
    assert_eq!(report["prepared_state_id"], current["state_id"]);
    assert_eq!(report["base_commit_sha"], current["source_commit_sha"]);
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "dirty not imported"
    );
}

#[test]
fn omp_mock_receives_literal_prompt_parses_bounded_jsonl_and_records_observed_version() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let profile = fixture.0.join("env-profile.json");
    fs::write(&profile, r#"{"harness_id":"omp","harness_version":"omp/19.1.0","connection_refs":[{"kind":"env","name":"GROQ_API_KEY"}]}"#).unwrap();
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("omp");
    fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
test -n "$HOME"
test -z "${UNSELECTED_SECRET+x}"
test -z "${OPENAI_API_KEY+x}"
test -z "${AWS_SECRET_ACCESS_KEY+x}"
test -z "${BASH_ENV+x}"
test -z "${PI_CODING_AGENT_DIR+x}"
if [ "$#" = 1 ] && [ "$1" = --version ]; then
    printf 'omp/19.1.0\n'
    exit 0
fi
test "$GROQ_API_KEY" = explicitly-authorized-fixture-key
test "$#" = 8
test "$1" = -p
test "$2" = --mode
test "$3" = json
test "$4" = --no-session
test "$5" = --no-title
test "$6" = --max-cost=0.25
test "$7" = --
test "$(cat .deps/ready)" = ready
printf '%s' "$8" > received.prompt
printf 'mock tracked change' > tracked
printf '{"type":"started","value":42}\r\nnot-json\n\377\n'
head -c 1048577 /dev/zero | tr '\000' x
printf '\n{"type":"finished"}\n{"last":true}'
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    for prompt in [
        "--model evil ; $(touch INJECTED) 'quoted' \"double\" \\\nsecond line\n",
        "@literal-file-not-an-option",
        "",
    ] {
        let prompt_path = fixture.0.join("omp.prompt");
        fs::write(&prompt_path, prompt).unwrap();
        let out = fixture
            .cli()
            .env("PATH", &path)
            .env("GROQ_API_KEY", "explicitly-authorized-fixture-key")
            .env("OPENAI_API_KEY", "must-not-leak")
            .env("AWS_SECRET_ACCESS_KEY", "must-not-leak")
            .env("BASH_ENV", "/must/not/load")
            .env("PI_CODING_AGENT_DIR", "/must/not/load")
            .args(["run", "test", "--harness", "omp", "--prompt-file"])
            .arg(prompt_path)
            .arg("--profile")
            .arg(&profile)
            .args([
                "--json",
                "--copy-limit-bytes",
                "10485760",
                "--max-cost-usd",
                "0.25",
            ])
            .output()
            .unwrap();
        let report = json(&out);
        assert_eq!(report["harness"], "omp");
        assert_eq!(report["harness_version"], "omp/19.1.0");
        assert_eq!(report["base_commit_sha"], base["source_commit_sha"]);
        assert_eq!(report["state"], "SUCCEEDED");
        assert_eq!(report["events"]["parsed"], 3);
        assert_eq!(report["events"]["invalid"], 2);
        assert_eq!(report["events"]["oversized"], 1);
        assert_eq!(report["worker_attempts"][0]["harness_requested"], "omp");
        assert_eq!(report["worker_attempts"][0]["harness_resolved"], "omp");
        assert_eq!(
            report["worker_attempts"][0]["harness_version"],
            report["harness_version"]
        );
        assert_eq!(
            report["worker_attempts"][0]["event_cursor"],
            report["capture"]["normalized_event_count"],
        );
        let tree = restored_workspace(&fixture, &report);
        let container = artifact_container(&fixture, &report);
        let records: Vec<serde_json::Value> = fs::read_to_string(container.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 8);
        assert_eq!(
            records
                .iter()
                .filter(|record| record.get("event").is_some())
                .count() as u64,
            report["capture"]["normalized_event_count"]
                .as_u64()
                .unwrap(),
        );
        assert_eq!(records.first().unwrap()["event"]["type"], "started");
        assert_eq!(records.last().unwrap()["event"]["type"], "completed");
        assert_eq!(records.last().unwrap()["event"]["success"], true);
        let records: Vec<_> = records
            .iter()
            .filter(|record| record["status"] != "normalized")
            .collect();
        assert_eq!(records.len(), 6);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record["line_number"], index + 1);
        }
        assert_eq!(records[0]["status"], "parsed");
        assert_eq!(records[0]["value"]["value"], 42);
        assert_eq!(records[1]["status"], "invalid");
        assert_eq!(records[1]["line_bytes"], 8);
        assert_eq!(records[2]["status"], "invalid");
        assert_eq!(records[2]["line_bytes"], 1);
        assert_eq!(records[3]["status"], "oversized");
        assert_eq!(records[3]["line_bytes"], 1048577);
        assert!(records[3].get("value").is_none());
        assert_eq!(records[4]["value"]["type"], "finished");
        assert_eq!(records[5]["value"]["last"], true);
        assert_eq!(report["capture"]["truncated"], false);
        assert_eq!(
            fs::read(tree.join("received.prompt")).unwrap(),
            prompt.as_bytes()
        );
        assert!(!tree.join("INJECTED").exists());
        assert_eq!(
            fs::read_to_string(tree.join("tracked")).unwrap(),
            "mock tracked change"
        );
        let run = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
            .unwrap()
            .run(&report["run_id"].as_str().unwrap().parse().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(run.harness_version, Some("omp/19.1.0".into()));
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(
            serde_json::to_value(run.capture.unwrap()).unwrap(),
            report["capture"]
        );
        assert_eq!(
            fs::read_to_string(repo.join("tracked")).unwrap(),
            "original"
        );
    }
}

#[test]
fn omp_large_native_records_survive_capture_normalization_and_sealing() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    fn padded(mut value: serde_json::Value, pointer: &str, bytes: usize) -> serde_json::Value {
        let overhead = serde_json::to_vec(&value).unwrap().len();
        *value.pointer_mut(pointer).unwrap() = serde_json::json!("x".repeat(bytes - overhead));
        assert_eq!(serde_json::to_vec(&value).unwrap().len(), bytes);
        value
    }

    let fixture = Fixture::new();
    fixture.setup();
    let turn = serde_json::json!({
        "type":"turn_end",
        "message":{"role":"assistant","content":[{"type":"text","text":""}],"stopReason":"stop"},
        "toolResults":[]
    });
    let values = [
        padded(turn.clone(), "/message/content/0/text", 108_091),
        serde_json::json!({"type":"message_end","message":{
            "role":"assistant","usage":{"input":7,"output":11,"cacheRead":3,"cost":{"total":0.01}}
        }}),
        padded(
            serde_json::json!({
                "type":"agent_end","isTerminal":true,
                "messages":[{"role":"assistant","content":[{"type":"text","text":""}],"stopReason":"stop"}]
            }),
            "/messages/0/content/0/text",
            185_472,
        ),
        padded(turn, "/message/content/0/text", 1024 * 1024),
        serde_json::json!({
            "type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"after-large"}
        }),
    ];
    let mut raw = Vec::new();
    let mut lengths = Vec::new();
    for (index, value) in values[..4].iter().enumerate() {
        let bytes = serde_json::to_vec(value).unwrap();
        lengths.push(bytes.len() + usize::from(index == 1));
        raw.extend_from_slice(&bytes);
        if index == 1 {
            raw.push(b'\r');
        }
        raw.push(b'\n');
    }
    raw.extend_from_slice(&serde_json::to_vec(&values[3]).unwrap());
    raw.push(b' '); // Valid JSON, one raw byte beyond the fixed record ceiling.
    raw.extend_from_slice(b"\n\xff\n");
    lengths.extend([1024 * 1024 + 1, 1]);
    let final_record = serde_json::to_vec(&values[4]).unwrap();
    lengths.push(final_record.len());
    raw.extend_from_slice(&final_record); // Final valid record has no LF.
    let input = fixture.0.join("native-records.jsonl");
    fs::write(&input, &raw).unwrap();
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("omp");
    fs::write(&executable, format!(
        "#!/bin/sh\nset -eu\nif [ \"$1\" = --version ]; then printf 'omp/19.1.0\\n'; exit 0; fi\nexec /bin/cat '{}'\n",
        input.display()
    )).unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    let profile = fixture.0.join("env-profile.json");
    fs::write(&profile, r#"{"harness_id":"omp","harness_version":"omp/19.1.0","connection_refs":[{"kind":"env","name":"GROQ_API_KEY"}]}"#).unwrap();
    let prompt = fixture.0.join("native.prompt");
    fs::write(&prompt, "provider-free record capture").unwrap();
    let report = json(
        &fixture
            .cli()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("GROQ_API_KEY", "fixture-key")
            .args(["run", "test", "--harness", "omp", "--prompt-file"])
            .arg(prompt)
            .arg("--profile")
            .arg(profile)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap(),
    );
    assert_eq!(report["state"], "SUCCEEDED");
    assert_eq!(report["harness_version"], "omp/19.1.0");
    assert_eq!(
        report["events"],
        serde_json::json!({"parsed":5,"invalid":1,"oversized":1})
    );
    let capture = &report["capture"];
    assert_eq!(capture["event_counts"], report["events"]);
    assert_eq!(capture["truncated"], false);
    assert!(capture["native_failure"].is_null());
    assert_eq!(capture["stdout_bytes_seen"], raw.len());
    assert_eq!(capture["stderr_bytes_seen"], 0);
    assert_eq!(capture["normalized_event_count"], 4);
    assert_eq!(report["worker_attempts"][0]["event_cursor"], 4);
    let container = artifact_container(&fixture, &report);
    assert_eq!(fs::read(container.join("stdout.log")).unwrap(), raw);
    let event_bytes = fs::read(container.join("events.jsonl")).unwrap();
    assert_eq!(event_bytes.last(), Some(&b'\n'));
    let records: Vec<serde_json::Value> = event_bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records.len(), 9);
    let native: Vec<_> = records
        .iter()
        .filter(|record| record["line_number"] != 0)
        .collect();
    assert_eq!(native.len(), 7);
    for (index, record) in native.iter().enumerate() {
        assert_eq!(record["line_number"], index + 1);
        assert_eq!(record["line_bytes"], lengths[index]);
        assert_eq!(
            record["status"],
            match index {
                4 => "oversized",
                5 => "invalid",
                _ => "parsed",
            }
        );
    }
    for index in 0..4 {
        assert_eq!(native[index]["value"], values[index]);
    }
    assert_eq!(native[6]["value"], values[4]);
    for index in [4, 5] {
        assert!(native[index].get("value").is_none());
        assert!(native[index].get("event").is_none());
    }
    for index in [0, 2, 3] {
        assert!(native[index].get("event").is_none()); // Snapshots are not invented output.
    }
    let normalized: Vec<_> = records
        .iter()
        .filter(|record| record.get("event").is_some())
        .collect();
    for (index, record) in normalized.iter().enumerate() {
        assert_eq!(record["sequence"], index + 1);
    }
    assert_eq!(normalized[0]["event"]["type"], "started");
    assert!(normalized[0]["event"]["pid"].as_u64().unwrap() > 0);
    assert_eq!(
        normalized[1]["event"],
        serde_json::json!({
            "type":"usage","input_tokens":7,"output_tokens":11,"cached_input_tokens":3,"cost_usd":0.01
        })
    );
    assert_eq!(
        normalized[2]["event"],
        serde_json::json!({
            "type":"output","stream":"stdout","text":"after-large"
        })
    );
    assert_eq!(
        normalized[3]["event"],
        serde_json::json!({
            "type":"completed","exit_code":0,"exit_signal":null,"success":true
        })
    );
    let mut total = 0;
    for (field, name) in [
        ("stdout", "stdout.log"),
        ("stderr", "stderr.log"),
        ("events", "events.jsonl"),
    ] {
        let bytes = fs::read(container.join(name)).unwrap();
        total += bytes.len();
        assert_eq!(capture[field]["size_bytes"], bytes.len());
        assert_eq!(
            capture[field]["sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
    }
    assert_eq!(capture["persisted_bytes"], total);
    assert!(total as u64 <= capture["limit_bytes"].as_u64().unwrap());
    assert_eq!(verified_manifest(&fixture, &report)["capture"], *capture);
}

#[test]
fn missing_omp_is_actionable_and_does_not_allocate_a_run() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("omp.prompt");
    fs::write(&prompt, "no model call").unwrap();
    let bin = fixture.0.join("empty-bin");
    fs::create_dir(&bin).unwrap();
    let out = fixture
        .cli()
        .env("PATH", bin)
        .args(["run", "test", "--harness", "omp", "--prompt-file"])
        .arg(prompt)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let diagnostic: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        diagnostic["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Harness executable 'omp' not found on PATH")
    );
    assert!(!fixture.0.join("storage/fork-staging").exists());
}

#[test]
fn hundred_mib_concurrent_fake_output_has_one_durable_capture_budget() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let binary = fixture.0.join("noisy-fake");
    fs::write(
        &binary,
        r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then printf 'fabric-fake-harness 1\n'; exit 0; fi
printf '{"type":"before","value":42}\nnot-json\n'
head -c 52428800 /dev/zero >&2 &
head -c 52428800 /dev/zero
printf '\n{"type":"after"}\n'
wait
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let prompt = fixture.0.join("noise.prompt");
    fs::write(&prompt, "{}").unwrap();
    let report = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(binary)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap(),
    );
    let container = artifact_container(&fixture, &report);
    let total: u64 = ["stdout.log", "stderr.log", "events.jsonl"]
        .iter()
        .filter_map(|name| fs::metadata(container.join(name)).ok())
        .map(|metadata| metadata.len())
        .sum();
    assert!(total <= 10 * 1024 * 1024, "persisted {total} capture bytes");
    let capture = &report["capture"];
    assert_eq!(capture["limit_bytes"], 10 * 1024 * 1024);
    assert_eq!(capture["persisted_bytes"], total);
    assert_eq!(capture["truncated"], true);
    assert_eq!(
        verified_manifest(&fixture, &report)["capture"]["truncated"],
        true
    );
    assert!(capture["stdout_bytes_seen"].as_u64().unwrap() >= 50 * 1024 * 1024);
    assert_eq!(capture["stderr_bytes_seen"], 50 * 1024 * 1024);
    assert_eq!(report["events"]["parsed"], 2);
    assert_eq!(report["events"]["invalid"], 1);
    assert_eq!(report["events"]["oversized"], 1);
    assert!(
        fs::read(container.join("stdout.log"))
            .unwrap()
            .ends_with(b"\n[FABRIC CAPTURE TRUNCATED]\n")
    );
    for (field, name) in [
        ("stdout", "stdout.log"),
        ("stderr", "stderr.log"),
        ("events", "events.jsonl"),
    ] {
        let bytes = fs::read(container.join(name)).unwrap();
        assert_eq!(capture[field]["size_bytes"], bytes.len());
        assert_eq!(
            capture[field]["sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
    }
    // Open a separate connection after the foreground command has exited.
    let reopened = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let run = reopened
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Succeeded);
    assert_eq!(
        serde_json::to_value(run.capture.unwrap()).unwrap(),
        *capture
    );
}

fn batch(fixture: &Fixture, manifest: &Path, concurrency: &str) -> Command {
    let mut command = fixture.cli();
    command
        .args(["run-many", "test", "--harness", "fake", "--tasks"])
        .arg(manifest)
        .args([
            "--concurrency",
            concurrency,
            "--copy-limit-bytes",
            "10485760",
            "--json",
        ]);
    command
}

#[test]
fn run_many_caps_actual_external_workers_and_durably_associates_results() {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("omp");
    let journal = fixture.0.join("activity");
    fs::write(
        &executable,
        format!(
            r#"#!/bin/sh
set -eu
lock='{}/lock'
journal='{}'
if [ "$1" = --version ]; then
  printf 'probe\n' >> "$journal.probes"; printf 'omp/19.1.0\n'; exit 0
fi
test "$(cat .deps/ready)" = ready
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
printf 'start %s\n' "$$" >> "$journal"
rmdir "$lock"
i=0
while [ "$(grep -c '^start ' "$journal")" -lt 2 ]; do
  i=$((i+1)); test "$i" -lt 500; sleep 0.01
done
sleep 0.1
printf '%s' "$7" > tracked
printf '{{"result":"%s"}}\n' "$7"
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
printf 'end %s\n' "$$" >> "$journal"
rmdir "$lock"
"#,
            fixture.0.display(),
            journal.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let prompts = fixture.0.join("manifests");
    fs::create_dir(&prompts).unwrap();
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            fs::write(prompts.join(format!("{i}.txt")), format!("task-{i}")).unwrap();
            serde_json::json!({"id":format!("task-{i}"), "prompt_file":format!("{i}.txt"),
                "request":{"profile":{"harness_id":"omp","harness_version":"omp/19.1.0",
                    "connection_refs":[{"kind":"env","name":"GROQ_API_KEY"}]}}})
        })
        .collect();
    let manifest = prompts.join("tasks.json");
    fs::write(&manifest, serde_json::to_vec(&tasks).unwrap()).unwrap();
    let out = fixture
        .cli()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("GROQ_API_KEY", "explicitly-authorized-fixture-key")
        .args(["run-many", "test", "--harness", "omp", "--tasks"])
        .arg(&manifest)
        .args([
            "--concurrency",
            "2",
            "--copy-limit-bytes",
            "10485760",
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        fs::read_to_string(journal.with_extension("probes")).unwrap(),
        "probe\n"
    );
    let report = json(&out);
    assert_eq!(report["succeeded"], true);
    let mut active = HashSet::new();
    let mut peak = 0;
    let mut starts = 0;
    for line in fs::read_to_string(journal).unwrap().lines() {
        let (event, pid) = line.split_once(' ').unwrap();
        if event == "start" {
            assert!(active.insert(pid.to_owned()));
            starts += 1;
            peak = peak.max(active.len());
        } else {
            assert!(active.remove(pid));
        }
        assert!(active.len() <= 2);
    }
    assert_eq!(starts, 4);
    assert_eq!(peak, 2);
    assert!(active.is_empty());
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let batch_id =
        fabric_domain::BatchId::try_from(report["batch_id"].as_str().unwrap().to_owned()).unwrap();
    let durable = store.batch_tasks(&batch_id).unwrap();
    assert_eq!(store.batch_succeeded(&batch_id).unwrap(), Some(true));
    assert_eq!(durable.len(), 4);
    let mut runs = HashSet::new();
    let mut workspaces = HashSet::new();
    for (i, task) in report["tasks"].as_array().unwrap().iter().enumerate() {
        assert_eq!(task["id"], format!("task-{i}"));
        let result = &task["result"];
        verified_manifest(&fixture, result);
        let run_id =
            fabric_domain::RunId::try_from(task["run_id"].as_str().unwrap().to_owned()).unwrap();
        assert!(runs.insert(run_id.clone()));
        assert_eq!(result["run_id"], task["run_id"]);
        let run = store.run(&run_id).unwrap().unwrap();
        assert_eq!(run.batch_id.as_ref(), Some(&batch_id));
        assert_eq!(run.prepared_state_id.as_str(), base["state_id"]);
        assert_eq!(run.state, fabric_domain::RunState::Succeeded);
        assert!(workspaces.insert(run.workspace_id.clone()));
        let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
        assert_eq!(workspace.state, fabric_domain::WorkspaceState::Reclaimed);
        assert!(!Path::new(&workspace.managed_path).exists());
        let restored = restored_workspace(&fixture, result);
        assert_eq!(
            fs::read_to_string(restored.join("tracked")).unwrap(),
            format!("task-{i}")
        );
        assert!(!restored.join(".deps").exists());
        assert_eq!(durable[i].task_id, task["id"]);
        assert_eq!(durable[i].run_id, run_id);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(durable[i].result.as_ref().unwrap()).unwrap(),
            *result
        );
        assert!(durable[i].error.is_none());
    }
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(Path::new(base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        "original"
    );
    assert_eq!(fixture.prepare()["state_id"], base["state_id"]);
}

#[test]
fn run_many_preserves_mixed_exit_and_prelaunch_failure_and_rejects_invalid_input() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let manifest = fixture.0.join("tasks.json");
    for exit in [0, 7] {
        fs::write(
            fixture.0.join(format!("{exit}.json")),
            serde_json::to_vec(&serde_json::json!({
                "required_files":[".deps/ready"], "exit_code":exit,
                "writes":[{"path":"tracked","content":format!("exit-{exit}")}]
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let probe_marker = fixture.0.join("unexpected-probe");
    let probe = fixture.0.join("probe");
    fs::write(
        &probe,
        format!("#!/bin/sh\ntouch '{}'\nexit 99\n", probe_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).unwrap();
    for (input, concurrency) in [
        ("[]", "2"),
        ("not json", "2"),
        (
            r#"[{"id":"a","prompt_file":"0.json"},{"id":"a","prompt_file":"7.json"}]"#,
            "2",
        ),
        (r#"[{"id":"","prompt_file":"0.json"}]"#, "2"),
        (r#"[{"id":" ","prompt_file":"0.json"}]"#, "2"),
        (r#"[{"id":"a","prompt_file":""}]"#, "2"),
        (r#"[{"prompt_file":"0.json"}]"#, "2"),
        (r#"[{"id":"a","prompt_file":7}]"#, "2"),
        (
            r#"[{"id":"a","prompt_file":"0.json","unexpected":true}]"#,
            "2",
        ),
        (r#"[{"id":"a","prompt_file":"0.json"}]"#, "0"),
        (r#"[{"id":"a","prompt_file":"0.json"}]"#, "-1"),
    ] {
        fs::write(&manifest, input).unwrap();
        let out = batch(&fixture, &manifest, concurrency)
            .arg("--fake-binary")
            .arg(&probe)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{out:?}");
        assert!(!fixture.0.join("storage/fork-staging").exists());
        assert!(
            !probe_marker.exists(),
            "invalid input launched an external probe"
        );
    }
    fs::write(&manifest, r#"[{"id":"ok","prompt_file":"0.json"},{"id":"nonzero","prompt_file":"7.json"},{"id":"missing","prompt_file":"missing.json"}]"#).unwrap();
    let out = batch(&fixture, &manifest, "2").output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["succeeded"], false);
    let tasks = report["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0]["result"]["exit_code"], 0);
    assert_eq!(tasks[0]["result"]["state"], "SUCCEEDED");
    assert_eq!(tasks[1]["result"]["exit_code"], 7);
    assert_eq!(tasks[1]["result"]["state"], "FAILED");
    assert_eq!(tasks[0]["result"]["worker_attempts"][0]["exit_code"], 0);
    assert_eq!(tasks[1]["result"]["worker_attempts"][0]["exit_code"], 7);
    assert!(tasks[2]["result"].is_null());
    assert!(tasks[2]["error"].as_str().unwrap().contains("missing.json"));
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let batch_id =
        fabric_domain::BatchId::try_from(report["batch_id"].as_str().unwrap().to_owned()).unwrap();
    let durable = store.batch_tasks(&batch_id).unwrap();
    assert_eq!(store.batch_succeeded(&batch_id).unwrap(), Some(false));
    assert_eq!(durable.len(), 3);
    assert!(store.run(&durable[2].run_id).unwrap().is_none());
    for (task, saved) in tasks.iter().zip(durable) {
        assert_eq!(saved.run_id.as_str(), task["run_id"]);
        assert_eq!(saved.task_id, task["id"]);
        if task["error"].is_null() {
            verified_manifest(&fixture, &task["result"]);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&saved.result.unwrap()).unwrap(),
                task["result"]
            );
        } else {
            assert_eq!(saved.error.as_deref(), task["error"].as_str());
        }
    }
}

#[test]
fn untracked_archive_restores_bytes_modes_links_and_excludes_ignored_dependencies() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (repo, _) = fixture.setup();
    let executable = fixture.script_fake(
        r#"
mkdir 'source dir'
chmod 750 'source dir'
printf '\000original\377' > 'source dir/file
with newline'
chmod 751 'source dir/file
with newline'
ln -s 'file
with newline' 'source dir/link'
printf 'ignored unique bytes' > .deps/ignored
printf 'tracked result' > tracked
/usr/bin/mkfifo .deps/ignored-pipe
mkdir 'source dir/long path'
i=0; name='source dir/long path/'; while [ "$i" -lt 120 ]; do name="${name}x"; i=$((i+1)); done
printf 'long path bytes' > "$name"
"#,
    );
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "archive fixture").unwrap();
    let report = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(executable)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap(),
    );
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let run = store
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    let files = store.artifacts(&run.run_id).unwrap();
    let archive = files
        .iter()
        .find(|file| file.kind == "untracked_archive")
        .expect("complete source capture must publish a restorable untracked archive");
    let archive = fixture.0.join("storage").join(&archive.relative_path);
    let bytes = fs::read(&archive).unwrap();
    assert!(
        !bytes
            .windows(b"ignored unique bytes".len())
            .any(|part| part == b"ignored unique bytes")
    );
    verified_manifest(&fixture, &report);
    assert_eq!(
        store.workspace(&run.workspace_id).unwrap().unwrap().state,
        fabric_domain::WorkspaceState::Reclaimed
    );
    // Restore into a fresh committed checkout after the worker tree was reclaimed.
    let restored = fixture.0.join("restored archive");
    fixture.git(
        &repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    success(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(&archive)
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read(restored.join("source dir/file\nwith newline")).unwrap(),
        b"\0original\xff"
    );
    assert_eq!(
        fs::metadata(restored.join("source dir/file\nwith newline"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        fs::metadata(restored.join("source dir"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(
        fs::read_link(restored.join("source dir/link")).unwrap(),
        Path::new("file\nwith newline")
    );
    let long = format!("source dir/long path/{}", "x".repeat(120));
    assert_eq!(fs::read(restored.join(long)).unwrap(), b"long path bytes");
    assert!(!restored.join(".deps").exists());
    assert_eq!(fs::read(repo.join("tracked")).unwrap(), b"original");
}

#[test]
fn tracked_binary_patch_round_trips_staged_and_unstaged_changes() {
    let fixture = Fixture::new();
    let (repo, _) = fixture.setup();
    fs::write(repo.join("binary"), b"\0original binary\xff").unwrap();
    fixture.git(&repo, &["add", "binary"]);
    fixture.git(&repo, &["commit", "--quiet", "-m", "binary baseline"]);
    fixture.prepare();
    let executable = fixture.script_fake(
        r#"
printf '\000staged\377' > binary
/usr/bin/git add binary
printf '\000unstaged final\376' > binary
printf '\000staged addition\375' > staged.bin
/usr/bin/git add staged.bin
printf 'unstaged text' > tracked
printf 'not captured' > untracked.txt
printf '{"captured":true}\n'
"#,
    );
    let prompt = fixture.0.join("binary-prompt");
    fs::write(&prompt, "exact prompt\n").unwrap();
    let report = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(executable)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap(),
    );
    verified_manifest(&fixture, &report);
    let patch = fixture
        .0
        .join("storage/artifacts")
        .join(report["artifact_manifest_id"].as_str().unwrap())
        .join("tracked.patch");
    let bytes = fs::read(&patch).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("GIT binary patch"));
    assert!(!String::from_utf8_lossy(&bytes).contains("untracked.txt"));
    let restored = fixture.0.join("restored");
    fixture.git(
        &repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    fixture.git(&restored, &["apply", "--binary", patch.to_str().unwrap()]);
    assert_eq!(
        fs::read(restored.join("binary")).unwrap(),
        b"\0unstaged final\xfe"
    );
    assert_eq!(
        fs::read(restored.join("staged.bin")).unwrap(),
        b"\0staged addition\xfd"
    );
    assert_eq!(
        fs::read(restored.join("tracked")).unwrap(),
        b"unstaged text"
    );
    assert_eq!(
        fs::read(repo.join("binary")).unwrap(),
        b"\0original binary\xff"
    );
    let tree = restored_workspace(&fixture, &report);
    assert_eq!(
        fs::read(tree.join("untracked.txt")).unwrap(),
        b"not captured"
    );
    let root = fixture.0.join("storage");
    let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
    assert!(
        db.execute(
            "UPDATE runs SET artifact_manifest_id='replacement' WHERE run_id=?1",
            [report["run_id"].as_str().unwrap()]
        )
        .is_err()
    );
    assert!(
        db.execute("UPDATE artifacts SET sha256=?1", ["0".repeat(64)])
            .is_err()
    );
}

#[test]
fn capture_failures_durably_retain_workspace_and_never_claim_terminal_success() {
    use fabric_domain::{RunState, WorkspaceState};
    for failure in [
        "promotion",
        "registration",
        "limit",
        "tampered_output",
        "outside_link",
        "relative_outside_link",
        "untracked_limit",
        "metadata_limit",
        "special_file",
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        let root = fixture.0.join("storage");
        let sentinel = fixture.0.join("outside-sentinel");
        fs::write(&sentinel, "outside secret target never captured").unwrap();
        let absolute_link = format!("/bin/ln -s '{}' outside-link", sentinel.display());
        let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
        if failure == "registration" {
            db.execute_batch(
                "CREATE TRIGGER injected_artifact_failure BEFORE INSERT ON artifacts
                BEGIN SELECT RAISE(ABORT, 'injected registration failure'); END;",
            )
            .unwrap();
        }
        let extra = match failure {
            "promotion" => {
                r#"
run=$(/usr/bin/sed -n '3p' ../.fabric-owner)
/bin/mkdir -p "../../../artifacts/a-$run"
printf 'existing sentinel' > "../../../artifacts/a-$run/sentinel"
"#
            }
            "tampered_output" => r#"/bin/rm ../stdout.log; /bin/ln -s /etc/passwd ../stdout.log"#,
            "outside_link" => absolute_link.as_str(),
            "relative_outside_link" => {
                "/bin/mkdir links; /bin/ln -s ../../../../../outside-sentinel links/outside-link"
            }
            "untracked_limit" => {
                "printf 'unique untracked recovery' > unique-large.bin; /usr/bin/head -c 65536 /dev/zero >> unique-large.bin"
            }
            "metadata_limit" => {
                "i=0; while [ \"$i\" -lt 40 ]; do : > \"empty-$i\"; i=$((i+1)); done"
            }
            "special_file" => "/usr/bin/mkfifo untracked-fifo",
            _ => "",
        };
        let executable = fixture.script_fake(&format!(
            "printf 'retained edit' > tracked\nprintf 'uncaptured' > untracked.txt\nprintf '{{\"event\":true}}\\n'\n{extra}"
        ));
        let prompt = fixture.0.join("prompt");
        fs::write(&prompt, "retained exact prompt").unwrap();
        let mut command = fixture.cli();
        command
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(executable)
            .arg("--prompt-file")
            .arg(&prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"]);
        if failure == "limit" {
            command.args(["--max-artifact-bytes", "32"]);
        }
        if matches!(failure, "untracked_limit" | "metadata_limit") {
            command.args(["--max-artifact-bytes", "16384"]);
        }
        let out = command.output().unwrap();
        assert!(!out.status.success(), "{failure}: {out:?}");
        let id: String = db
            .query_row("SELECT run_id FROM runs", [], |row| row.get(0))
            .unwrap();
        drop(db);
        let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
        let id = id.parse().unwrap();
        let run = store.run(&id).unwrap().unwrap();
        assert_eq!(run.state, RunState::NeedsAttention, "{failure}: {out:?}");
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.exit_signal, None);
        let attempts = store.worker_attempts(&id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].exit_code, Some(0));
        assert_eq!(attempts[0].exit_signal, None);
        assert_eq!(attempts[0].termination_reason, None);
        assert!(attempts[0].finished_at.is_some());
        assert_eq!(run.error_kind.as_deref(), Some("artifact_capture"));
        assert!(run.finished_at.is_some());
        assert!(run.artifact_manifest_id.is_none());
        assert!(run.capture.is_some());
        assert!(store.artifacts(&id).unwrap().is_empty());
        let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
        assert_eq!(workspace.state, WorkspaceState::RecoveryRequired);
        let tree = Path::new(&workspace.managed_path);
        assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"retained edit");
        assert_eq!(fs::read(tree.join("untracked.txt")).unwrap(), b"uncaptured");
        assert_eq!(
            fs::read(&sentinel).unwrap(),
            b"outside secret target never captured"
        );
        if failure == "untracked_limit" {
            let mut expected = b"unique untracked recovery".to_vec();
            expected.resize(expected.len() + 65536, 0);
            assert_eq!(fs::read(tree.join("unique-large.bin")).unwrap(), expected);
        }
        if matches!(failure, "untracked_limit" | "metadata_limit") {
            let stage = root.join(format!("artifact-staging/a-{id}"));
            let size: u64 = fs::read_dir(stage)
                .unwrap()
                .map(|entry| entry.unwrap().metadata().unwrap().len())
                .sum();
            assert!(
                size <= 16384,
                "tar metadata and partial output must respect the cap"
            );
        }
        if matches!(failure, "outside_link" | "relative_outside_link") {
            assert!(
                workspace
                    .retention_reason
                    .as_deref()
                    .unwrap()
                    .contains("symlink")
            );
            let stage = root.join(format!("artifact-staging/a-{id}"));
            for file in fs::read_dir(stage).unwrap() {
                let bytes = fs::read(file.unwrap().path()).unwrap();
                assert!(
                    !bytes
                        .windows(b"outside secret target never captured".len())
                        .any(|part| part == b"outside secret target never captured")
                );
            }
        }
        assert_eq!(
            fs::read(tree.parent().unwrap().join("prompt.input")).unwrap(),
            fs::read(&prompt).unwrap()
        );
        assert!(tree.parent().unwrap().join("stderr.log").is_file());
        if failure == "registration" {
            let manifest = root.join(format!("artifacts/a-{id}/manifest.json"));
            assert!(
                manifest.is_file(),
                "promoted recovery artifact must survive DB rollback"
            );
        } else {
            assert!(root.join(format!("artifact-staging/a-{id}")).is_dir());
        }
        if failure == "promotion" {
            assert_eq!(
                fs::read(root.join(format!("artifacts/a-{id}/sentinel"))).unwrap(),
                b"existing sentinel"
            );
        }
    }
}

#[test]
fn storage_admission_cli_denies_before_worker_and_reports_usage() {
    let fixture = Fixture::new();
    fixture.setup();
    let launched = fixture.0.join("launched");
    let fake = fixture.script_fake(&format!("touch '{}'", launched.display()));
    let prompt = fixture.0.join("storage-prompt");
    fs::write(&prompt, "{}").unwrap();
    let output = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(fake)
        .arg("--prompt-file")
        .arg(prompt)
        .args(["--min-free-bytes", "18446744073709551615", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let failure: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("storage_admission"),
        "{failure}"
    );
    assert!(!launched.exists());
    assert!(!fixture.0.join("storage/fork-staging").exists());
    let status = json(
        &fixture
            .cli()
            .args(["storage", "status", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        status["accounting"],
        "logical_bytes_best_effort_not_exclusive"
    );
    assert!(status["managed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(status["caches"][0]["retention_reason"], "selected_state");
    let manifest = fixture.0.join("tasks.json");
    fs::write(&manifest, r#"[{"id":"one","prompt_file":"storage-prompt"},{"id":"two","prompt_file":"storage-prompt"}]"#).unwrap();
    let output = batch(&fixture, &manifest, "2")
        .arg("--fake-binary")
        .arg(fixture.0.join("fake-script"))
        .args([
            "--max-managed-bytes",
            "3145728",
            "--min-free-bytes",
            "1",
            "--max-workspace-bytes",
            "1048576",
            "--max-log-bytes",
            "1048576",
            "--max-artifact-bytes",
            "1048576",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["error"]
                .as_str()
                .is_some_and(|message| message.contains("storage_admission"))),
        "{report}"
    );
    assert!(!launched.exists());
    let recipe = fixture.0.join("denied-recipe.toml");
    fs::write(
        &recipe,
        format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',\"touch '{}'\"]\n",
            launched.display()
        ),
    )
    .unwrap();
    let output = fixture
        .cli()
        .args(["project", "prepare", "test", "--recipe"])
        .arg(&recipe)
        .args(["--min-free-bytes", "18446744073709551615", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("storage_admission")
    );
    assert!(!launched.exists());
}

struct FixedDisk(u64);
impl fabric_core::workspace::storage::DiskMeter for FixedDisk {
    fn free_bytes(&self, _: &Path) -> Result<u64, fabric_core::workspace::storage::MeterCause> {
        Ok(self.0)
    }
}

fn storage_options(fixture: &Fixture) -> fabric_core::run::RunOptions {
    let mut options = fabric_core::run::RunOptions {
        copy_limit_bytes: 10 * 1024 * 1024,
        max_log_bytes: 1024 * 1024,
        max_artifact_bytes: 1024 * 1024,
        ..fixture.options()
    };
    options.storage.policy.max_workspace_bytes = 128 * 1024;
    options.storage.policy.min_free_bytes = 1;
    options.storage.meter = std::sync::Arc::new(FixedDisk(u64::MAX));
    options
}

#[test]
fn storage_injected_low_free_and_managed_cap_never_launch() {
    let fixture = Fixture::new();
    fixture.setup();
    let marker = fixture.0.join("launched");
    let harness = fabric_runner::FakeHarness::new(
        fixture.script_fake(&format!("touch '{}'", marker.display())),
    )
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options(&fixture);
    options.storage.meter = std::sync::Arc::new(FixedDisk(0));
    let denied = fabric_core::run::foreground(
        &fixture.0.join("storage"),
        "test",
        &prompt,
        &harness,
        &options,
    )
    .err()
    .unwrap();
    assert!(denied.message.contains("storage_admission"), "{denied}");
    let recipe = fixture.0.join("denied-prepare.toml");
    fs::write(
        &recipe,
        format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',\"touch '{}'\"]\n",
            marker.display()
        ),
    )
    .unwrap();
    let denied = fabric_core::workspace::storage::prepare(
        &fixture.0.join("storage"),
        "test",
        &recipe,
        &options.storage,
    )
    .unwrap_err();
    assert!(denied.message.contains("storage_admission"));
    options.storage.meter = std::sync::Arc::new(FixedDisk(u64::MAX));
    options.storage.policy.max_managed_bytes = options.storage.policy.max_workspace_bytes
        + options.max_log_bytes
        + options.max_artifact_bytes;
    let denied = fabric_core::run::foreground(
        &fixture.0.join("storage"),
        "test",
        &prompt,
        &harness,
        &options,
    )
    .err()
    .unwrap();
    assert!(denied.message.contains("storage_admission"), "{denied}");
    assert!(!marker.exists());
    assert!(
        fabric_core::workspace::storage::status(&fixture.0.join("storage"), &options.storage)
            .unwrap()
            .workspaces
            .is_empty()
    );
}

#[test]
fn storage_concurrent_workers_share_atomic_admission() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let marker = fixture.0.join("launched");
    let harness = fabric_runner::FakeHarness::new(
        fixture.script_fake(&format!("echo launched >> '{}'; sleep 1", marker.display())),
    )
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options(&fixture);
    let reservation = options.storage.policy.max_workspace_bytes
        + options.max_log_bytes
        + options.max_artifact_bytes;
    options.storage.policy.max_managed_bytes =
        fabric_core::workspace::storage::usage(&root).unwrap() + reservation + 512 * 1024;
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    fabric_core::run::foreground(&root, "test", &prompt, &harness, &options)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.message.contains("storage_admission"))
    );
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert_eq!(
        fabric_core::workspace::storage::status(&root, &options.storage)
            .unwrap()
            .reserved_bytes,
        0
    );
}

#[test]
fn storage_evicts_only_unreferenced_cache_and_same_key_rebuilds() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let recipe = fixture.0.join("large-recipe.toml");
    fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','dd if=/dev/zero of=cache bs=1048576 count=2 2>/dev/null']\n").unwrap();
    let old = fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
        .unwrap();
    // A new recipe for the same committed tree becomes the selected state.
    fs::write(
        &recipe,
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf selected > selected']\n",
    )
    .unwrap();
    let selected =
        fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
            .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake("exit 0")).unwrap();
    let mut options = storage_options(&fixture);
    let reservation = options.storage.policy.max_workspace_bytes
        + options.max_log_bytes
        + options.max_artifact_bytes;
    options.storage.policy.max_managed_bytes =
        fabric_core::workspace::storage::usage(&root).unwrap() + reservation - 1024 * 1024;
    let report = fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).unwrap();
    assert_eq!(report.state, fabric_domain::RunState::Succeeded);
    assert!(!Path::new(&old.prepared_path).exists());
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    assert!(store.prepared_state(&old.state_id).unwrap().is_none());
    assert!(Path::new(&selected.prepared_path).exists());
    assert_eq!(
        store
            .cache_retention(&selected.state_id)
            .unwrap()
            .as_deref(),
        Some("historical_reference")
    );
    fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','dd if=/dev/zero of=cache bs=1048576 count=2 2>/dev/null']\n").unwrap();
    let rebuilt =
        fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
            .unwrap();
    assert_eq!(rebuilt.preparation_key, old.preparation_key);
    assert_ne!(rebuilt.state_id, old.state_id);
    fabric_project::prepare::validate_ready(&root, &rebuilt).unwrap();
    options.storage.policy.max_managed_bytes = reservation;
    assert!(fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).is_err());
    assert!(Path::new(&rebuilt.prepared_path).exists());
    assert!(Path::new(&selected.prepared_path).exists());
}

#[test]
fn storage_prepare_reuses_validated_ready_identity_without_fresh_admission() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let recipe = fixture.0.join("counted-recipe.toml");
    let counter = fixture.0.join("setup-count");
    fs::write(
        &recipe,
        format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',\"printf x >> '{}'; printf cached > cached\"]\n",
            counter.display()
        ),
    )
    .unwrap();
    let original =
        fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
            .unwrap();
    // A different selection makes the requested cache otherwise eligible for eviction.
    let selection = fixture.0.join("selection.toml");
    fs::write(
        &selection,
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf selected > selected']\n",
    )
    .unwrap();
    fabric_core::workspace::storage::prepare(&root, "test", &selection, &Default::default())
        .unwrap();
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    assert_eq!(store.cache_retention(&original.state_id).unwrap(), None);
    let identity = fs::metadata(&original.prepared_path).unwrap();
    for free in [0, u64::MAX] {
        let mut options = fabric_core::workspace::storage::StorageOptions {
            meter: std::sync::Arc::new(FixedDisk(free)),
            ..Default::default()
        };
        options.policy.max_managed_bytes =
            fabric_core::workspace::storage::usage(&root).unwrap() + 4096;
        options.policy.max_workspace_bytes = options.policy.max_managed_bytes - 2;
        options.policy.max_log_bytes_per_run = 1;
        options.policy.max_artifact_bytes_per_run = 1;
        let reused =
            fabric_core::workspace::storage::prepare(&root, "test", &recipe, &options).unwrap();
        assert_eq!(reused.state_id, original.state_id);
        assert_eq!(reused.prepared_path, original.prepared_path);
        let current = fs::metadata(&reused.prepared_path).unwrap();
        assert_eq!(
            (current.dev(), current.ino()),
            (identity.dev(), identity.ino())
        );
        assert_eq!(fs::read(&counter).unwrap(), b"x");
        fabric_project::prepare::validate_ready(&root, &reused).unwrap();
    }
}

#[test]
fn storage_workspace_monitor_terminates_owned_group_and_zero_exit_is_failure() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let child_pid = fixture.0.join("child-pid");
    let binary = fixture.script_fake(&format!("trap 'exit 0' TERM\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\ndd if=/dev/zero of=oversized bs=1048576 count=3 2>/dev/null\nwhile :; do sleep 1; done", child_pid.display()));
    let harness = fabric_runner::FakeHarness::new(binary).unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options(&fixture);
    options.storage.policy.max_workspace_bytes = 1024 * 1024;
    options.max_artifact_bytes = 8 * 1024 * 1024;
    options.limits.term_grace = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();
    let report = fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(report.state, fabric_domain::RunState::Failed);
    assert_eq!(report.error_kind.as_deref(), Some("workspace_limit"));
    assert_eq!(report.exit_code, Some(0));
    assert!(!options.cancellation.is_cancelled());
    let run = fabric_store::Store::open(root.join("fabric.db"))
        .unwrap()
        .run(&report.run_id.parse().unwrap())
        .unwrap()
        .unwrap();
    assert!(run.artifact_manifest_id.is_some());
    assert_eq!(run.error_kind.as_deref(), Some("workspace_limit"));
    let pid = fs::read_to_string(child_pid).unwrap();
    let state = Command::new("/bin/ps")
        .args(["-p", &pid, "-o", "stat="])
        .output()
        .unwrap();
    let state = String::from_utf8(state.stdout).unwrap();
    assert!(
        state.trim().is_empty() || state.trim().starts_with('Z'),
        "owned descendant survived: {state}"
    );
}

struct PressureDisk(PathBuf);
impl fabric_core::workspace::storage::DiskMeter for PressureDisk {
    fn free_bytes(&self, _: &Path) -> Result<u64, fabric_core::workspace::storage::MeterCause> {
        Ok(if self.0.exists() { 0 } else { u64::MAX })
    }
}

#[test]
fn storage_pressure_keeps_its_reason_and_captures_evidence() {
    let fixture = Fixture::new();
    fixture.setup();
    let pressure = fixture.0.join("pressure");
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake(&format!(
        "trap 'exit 0' TERM\nprintf evidence > tracked\ntouch '{}'\nwhile :; do sleep 1; done",
        pressure.display()
    )))
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options(&fixture);
    options.storage.meter = std::sync::Arc::new(PressureDisk(pressure));
    options.limits.term_grace = std::time::Duration::from_secs(2);
    let report = fabric_core::run::foreground(
        &fixture.0.join("storage"),
        "test",
        &prompt,
        &harness,
        &options,
    )
    .unwrap();
    assert_eq!(report.state, fabric_domain::RunState::Failed);
    assert_eq!(report.error_kind.as_deref(), Some("storage_pressure"));
    assert_eq!(report.exit_code, Some(0));
    assert!(!options.cancellation.is_cancelled());
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    assert!(
        store
            .run(&report.run_id.parse().unwrap())
            .unwrap()
            .unwrap()
            .artifact_manifest_id
            .is_some()
    );
}

struct FailingDisk(PathBuf);
impl fabric_core::workspace::storage::DiskMeter for FailingDisk {
    fn free_bytes(&self, _: &Path) -> Result<u64, fabric_core::workspace::storage::MeterCause> {
        use fabric_core::workspace::storage::{MeterCause, MeterOperation};
        if self.0.exists() {
            Err(MeterCause::Io {
                operation: MeterOperation::Statvfs,
                os_code: Some(libc::EIO),
            })
        } else {
            Ok(u64::MAX)
        }
    }
}

#[test]
fn storage_meter_failure_survives_terminal_reconnect_without_raw_content() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let failure = fixture.0.join("credential-path-sentinel");
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake(&format!(
        "trap 'exit 0' TERM\nprintf file-content-sentinel > tracked\nprintf 'native-raw-sentinel\\n'\ntouch '{}'\nwhile :; do sleep 1; done",
        failure.display()
    )))
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options(&fixture);
    options.storage.meter = std::sync::Arc::new(FailingDisk(failure));
    options.limits.term_grace = std::time::Duration::from_secs(2);
    let report = fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).unwrap();
    assert_eq!(report.state, fabric_domain::RunState::Failed);
    assert_eq!(report.error_kind.as_deref(), Some("storage_meter_failed"));
    assert_eq!(
        report.exit_code,
        Some(0),
        "storage failure overrides zero exit"
    );
    assert!(!options.cancellation.is_cancelled());
    assert!(report.artifact_manifest_id.is_some());
    assert!(
        !Path::new(&report.workspace_path).exists(),
        "captured fork reclaimed"
    );
    let expected = serde_json::json!({
        "stage":"free_space", "code":"io", "operation":"statvfs", "os_code":libc::EIO
    });
    assert_eq!(
        serde_json::to_value(report.storage_meter_failure).unwrap(),
        expected
    );

    // New viewer process, reading durable metadata after the supervisor has returned.
    let canonical_root = root.canonicalize().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fabric"))
        .env_clear()
        .arg("--root")
        .arg(canonical_root)
        .args(["runs", "logs", &report.run_id, "--json"])
        .output()
        .unwrap();
    let reconnected = json(&output);
    assert_eq!(reconnected["terminal"], true);
    assert_eq!(reconnected["state"], "FAILED");
    assert_eq!(reconnected["termination"], "storage_meter_failed");
    assert_eq!(reconnected["workspace_state"], "RECLAIMED");
    assert_eq!(reconnected["progress"]["storage_meter_failure"], expected);
    assert!(reconnected["raw"].is_null());
    let visible = String::from_utf8(output.stdout).unwrap();
    for private in [
        "credential-path-sentinel",
        "native-raw-sentinel",
        "file-content-sentinel",
    ] {
        assert!(!visible.contains(private), "private content in observation");
    }
    let store = fabric_store::Store::open_existing_read_only(root.join("fabric.db")).unwrap();
    assert_eq!(store.reserved_storage_bytes().unwrap(), 0);
}

#[test]
fn storage_fast_zero_exit_overlimit_retains_reason_when_capture_exceeds_cap() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake(
        "printf 'unique recovery bytes' > tracked\ndd if=/dev/zero of=oversized bs=1048576 count=3 2>/dev/null\nexit 0"
    )).unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options(&fixture);
    options.storage.policy.max_workspace_bytes = 1024 * 1024;
    assert!(fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).is_err());
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    let id = store.workspace_ids().unwrap().pop().unwrap();
    let workspace = store.workspace(&id).unwrap().unwrap();
    let run = store.run(&workspace.run_id).unwrap().unwrap();
    assert_eq!(run.state, fabric_domain::RunState::NeedsAttention);
    assert_eq!(run.error_kind.as_deref(), Some("workspace_limit"));
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(
        workspace.state,
        fabric_domain::WorkspaceState::RecoveryRequired
    );
    assert_eq!(
        fs::read(Path::new(&workspace.managed_path).join("tracked")).unwrap(),
        b"unique recovery bytes"
    );
    let status = fabric_core::workspace::storage::status(&root, &options.storage).unwrap();
    assert!(status.workspaces[0].usage_bytes.unwrap() > 3 * 1024 * 1024);
    assert!(!status.workspaces[0].retention_reason.is_empty());
}

#[test]
fn acceptance_cli_inspects_four_isolated_runs_and_recovery_after_reclaim() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let (repo, _) = fixture.setup();
    let recipe = fixture.0.join("acceptance.toml");
    fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir -p .deps; printf ready > .deps/ready; printf x >> .deps/setup-count']\n").unwrap();
    let prepare = || {
        json(
            &fixture
                .cli()
                .args(["project", "prepare", "test", "--recipe"])
                .arg(&recipe)
                .args(["--json"])
                .output()
                .unwrap(),
        )
    };
    let base = prepare();
    assert_eq!(prepare(), base);
    let base_path = Path::new(base["prepared_path"].as_str().unwrap());
    assert_eq!(fs::read(base_path.join(".deps/setup-count")).unwrap(), b"x");
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            let prompt = format!("acceptance-{i}.json");
            fs::write(
                fixture.0.join(&prompt),
                serde_json::to_vec(&serde_json::json!({
                    "required_files":[".deps/ready", ".deps/setup-count"],
                    "writes":[{"path":"tracked","content":format!("tracked-{i}")},
                              {"path":"new-source","content":format!("new-{i}")}],
                    "events":[{"type":"acceptance","worker":i}],
                    "absent_environment":["UNSELECTED_SECRET"]
                }))
                .unwrap(),
            )
            .unwrap();
            serde_json::json!({"id":format!("worker-{i}"),"prompt_file":prompt})
        })
        .collect();
    let manifest = fixture.0.join("acceptance-tasks.json");
    fs::write(&manifest, serde_json::to_vec(&tasks).unwrap()).unwrap();
    let report = json(&batch(&fixture, &manifest, "2").output().unwrap());
    let listed = json(
        &fixture
            .cli()
            .args(["runs", "list", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(listed.as_array().unwrap().len(), 4);
    for (i, task) in report["tasks"].as_array().unwrap().iter().enumerate() {
        let id = task["run_id"].as_str().unwrap();
        let inspect = || {
            fixture
                .cli()
                .args(["runs", "show", id, "--json"])
                .output()
                .unwrap()
        };
        let first = inspect();
        let shown = json(&first);
        assert_eq!(first.stdout, inspect().stdout);
        assert_eq!(shown["run"]["state"], "SUCCEEDED");
        assert_eq!(shown["run"]["batch_id"], report["batch_id"]);
        assert_eq!(shown["workspace"]["state"], "RECLAIMED");
        assert!(shown["workspace"]["retention_reason"].is_null());
        assert!(!Path::new(shown["workspace"]["managed_path"].as_str().unwrap()).exists());
        assert_eq!(
            shown["prepared_state"]["recipe_digest"],
            base["recipe_digest"]
        );
        assert_eq!(shown["run"]["base_commit_sha"], base["source_commit_sha"]);
        assert_eq!(shown["run"]["harness_version"], "fabric-fake-harness 1");
        assert_eq!(shown["run"]["exit_code"], 0);
        assert!(shown["run"]["pid_metadata"].is_string());
        assert!(shown["run"]["finished_at"].is_number());
        let files = json(
            &fixture
                .cli()
                .args(["runs", "artifacts", id, "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(files["run_id"], id);
        assert_eq!(
            files["artifact_manifest_id"],
            shown["run"]["artifact_manifest_id"]
        );
        for file in files["artifacts"].as_array().unwrap() {
            let bytes = fs::read(
                fixture
                    .0
                    .join("storage")
                    .join(file["relative_path"].as_str().unwrap()),
            )
            .unwrap();
            assert_eq!(file["size_bytes"], bytes.len() as u64);
            assert_eq!(file["sha256"], format!("{:x}", Sha256::digest(bytes)));
        }
        let recovered = restored_workspace(&fixture, &task["result"]);
        assert_eq!(
            fs::read_to_string(recovered.join("tracked")).unwrap(),
            format!("tracked-{i}")
        );
        assert_eq!(
            fs::read_to_string(recovered.join("new-source")).unwrap(),
            format!("new-{i}")
        );
        assert!(!recovered.join(".deps").exists());
        verified_manifest(&fixture, &task["result"]);
        success(&fixture.cli().args(["runs", "show", id]).output().unwrap());
        success(
            &fixture
                .cli()
                .args(["runs", "artifacts", id])
                .output()
                .unwrap(),
        );
    }
    assert_eq!(fs::read(repo.join("tracked")).unwrap(), b"original");
    assert_eq!(fs::read(base_path.join("tracked")).unwrap(), b"original");
    assert_eq!(fs::read(base_path.join(".deps/setup-count")).unwrap(), b"x");
    assert!(!base_path.join("new-source").exists());
    assert!(
        fs::read_dir(fixture.0.join("storage/fork-staging"))
            .unwrap()
            .next()
            .is_none()
    );
    let project = json(
        &fixture
            .cli()
            .args(["project", "status", "test", "--json"])
            .output()
            .unwrap(),
    );
    assert!(
        project["prepared_states"]
            .as_array()
            .unwrap()
            .iter()
            .any(|state| state["state_id"] == base["state_id"] && state["status"] == "READY")
    );
    for command in [
        ["runs", "show", "missing", "--json"],
        ["runs", "artifacts", "missing", "--json"],
    ] {
        let out = fixture.cli().args(command).output().unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        let error: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "not_found");
    }
    let out = fixture
        .cli()
        .args(["runs", "show", "../escape", "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        json(
            &fixture
                .cli()
                .args(["storage", "status", "--json"])
                .output()
                .unwrap()
        )["managed_bytes"]
            .is_number()
    );
    assert_eq!(gc(&fixture, "--dry-run")["mode"], "dry_run");
    assert_eq!(gc(&fixture, "--apply")["mode"], "apply");
}

#[test]
fn inspection_reads_live_wal_without_migration_or_logical_writes() {
    let fixture = Fixture::new();
    fixture.setup();
    let report = pending_workspace(&fixture);
    let id = report["run_id"].as_str().unwrap();
    let database = fixture.0.join("storage/fabric.db");
    let wal = fixture.0.join("storage/fabric.db-wal");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let before_db = fs::read(&database).unwrap();
    db.execute_batch("PRAGMA user_version=7; UPDATE workspaces SET retention_reason='live WAL evidence', lease_expires_at=9223372036854775807;").unwrap();
    assert_eq!(fs::read(&database).unwrap(), before_db);
    let before_wal = fs::read(&wal).unwrap();
    assert!(before_wal.len() > 32);
    let commands: Vec<Vec<&str>> = vec![
        vec!["doctor", "--json"],
        vec!["project", "status", "test", "--json"],
        vec!["runs", "list", "--json"],
        vec!["runs", "show", id, "--json"],
        vec!["runs", "artifacts", id, "--json"],
        vec!["storage", "status", "--json"],
        vec!["storage", "gc", "--dry-run", "--json"],
    ];
    for command in commands {
        let first = fixture.cli().args(&command).output().unwrap();
        let value = json(&first);
        let second = fixture.cli().args(&command).output().unwrap();
        let repeated = json(&second);
        if command[..2] != ["storage", "status"] {
            assert_eq!(first.stdout, second.stdout, "{command:?}");
        } else {
            // Native free-space measurements can change independently of Fabric.
            assert_eq!(value["workspaces"], repeated["workspaces"]);
            assert_eq!(value["policy"], repeated["policy"]);
        }
        if command[..2] == ["runs", "show"] {
            assert_eq!(value["workspace"]["retention_reason"], "live WAL evidence");
            assert_eq!(value["workspace"]["lease_expires_at"], i64::MAX);
        }
        assert!(
            !String::from_utf8(first.stdout)
                .unwrap()
                .contains("must-not-leak")
        );
    }
    for command in ["show", "artifacts"] {
        let out = fixture
            .cli()
            .args(["runs", command, "unknown", "--json"])
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
    assert_eq!(fs::read(&database).unwrap(), before_db);
    assert_eq!(fs::read(&wal).unwrap(), before_wal);
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM cleanup_intents", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"worker tracked");
    assert_eq!(
        fs::read(tree.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
}

#[test]
fn native_profiles_record_immutable_nonsecret_evidence_through_single_and_batch_paths() {
    let fixture = Fixture::new();
    fixture.setup();
    let profile = fixture.native_profile();
    let profile_file = fixture.0.join("profile.json");
    let prompt = fixture.0.join("native-prompt");
    fs::write(&prompt, "Fixture only; never call a model").unwrap();
    let tasks = fixture.0.join("native-tasks.json");
    let mut previous = None;
    for mode in ["inline", "single", "batch-default", "changed-config"] {
        let mut selected = profile.clone();
        if mode == "single" {
            selected["connection_refs"] = serde_json::json!([{"kind":"native-auth","name":"native-home"},{"kind":"env","name":"OPENAI_API_KEY"}]);
        }
        if mode == "changed-config" {
            fs::write(
                fixture.0.join("selected-home/.omp/agent/config.yml"),
                "theme: light\napiKey: config-secret-sentinel\n",
            )
            .unwrap();
        }
        fs::write(&profile_file, serde_json::to_vec(&selected).unwrap()).unwrap();
        let mut command = fixture.native_cli();
        if mode == "single" {
            command
                .args(["run", "test", "--harness", "omp", "--prompt-file"])
                .arg(&prompt)
                .arg("--profile")
                .arg(&profile_file);
        } else {
            let mut task =
                serde_json::json!({"id":"native","prompt_file":"native-prompt","harness":"omp"});
            if mode != "batch-default" {
                task["request"] = serde_json::json!({"profile":selected});
            }
            fs::write(&tasks, serde_json::to_vec(&vec![task]).unwrap()).unwrap();
            command
                .args(["run-many", "test", "--tasks"])
                .arg(&tasks)
                .args(["--concurrency", "1"]);
            if mode == "batch-default" {
                command.arg("--profile").arg(&profile_file);
            }
        }
        let output = command
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap();
        let report = json(&output);
        let run = if mode == "single" {
            &report
        } else {
            &report["tasks"][0]["result"]
        };
        let evidence = &run["profile"];
        let digest = evidence["digest"].as_str().unwrap();
        assert_eq!(digest.len(), 64);
        assert_eq!(
            evidence["native_config_fingerprint"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(evidence["portable"], false);
        assert!(!evidence["diagnostics"].as_array().unwrap().is_empty());
        assert_eq!(run["worker_attempts"][0]["profile_digest"], digest);
        assert_eq!(verified_manifest(&fixture, run)["profile"], *evidence);
        if mode == "inline" {
            previous = Some(digest.to_owned());
        }
        if mode == "batch-default" {
            assert_eq!(previous.as_deref(), Some(digest));
        }
        if mode == "changed-config" {
            assert_ne!(previous.as_deref(), Some(digest));
        }
        for secret in [
            "authorized-key-secret-sentinel",
            "opaque-auth-secret-sentinel",
            "config-secret-sentinel",
        ] {
            assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
            for file in fs::read_dir(artifact_container(&fixture, run)).unwrap() {
                assert!(
                    !String::from_utf8_lossy(&fs::read(file.unwrap().path()).unwrap())
                        .contains(secret)
                );
            }
            assert!(
                !String::from_utf8_lossy(&fs::read(fixture.0.join("storage/fabric.db")).unwrap())
                    .contains(secret)
            );
        }
    }
}

#[test]
fn invalid_profiles_and_late_batch_requirements_are_secret_safe_and_atomic() {
    let fixture = Fixture::new();
    fixture.setup();
    let base = fixture.native_profile();
    let profile_file = fixture.0.join("profile.json");
    let prompt = fixture.0.join("native-prompt");
    fs::write(&prompt, "fixture only").unwrap();
    let before = fixture.inventory();
    let mut invalid = Vec::new();
    for (field, value) in [
        ("harness_id", serde_json::json!("fake-a")),
        ("harness_version", serde_json::json!("omp/0.0.0")),
        ("skills", serde_json::json!(["unsupported-skill"])),
        ("plugins", serde_json::json!(["unsupported-plugin"])),
        ("mcp", serde_json::json!(["unsupported-mcp"])),
        ("connection_refs", serde_json::json!([])),
        (
            "native_config_refs",
            serde_json::json!([{"kind":"unknown","path":fixture.0}]),
        ),
        (
            "connection_refs",
            serde_json::json!([{"kind":"unknown","name":"unknown"}]),
        ),
        (
            "connection_refs",
            serde_json::json!([{"kind":"env","name":"NOT_APPROVED"}]),
        ),
        (
            "connection_refs",
            serde_json::json!([{"kind":"env","name":"ANTHROPIC_API_KEY"}]),
        ),
        ("raw_credential", serde_json::json!("parse-secret-sentinel")),
        ("parse-secret-sentinel", serde_json::json!("unknown")),
        (
            "native_config_refs",
            serde_json::json!([{"kind":"native-home","path":fixture.0,"secret":"parse-secret-sentinel"}]),
        ),
        (
            "connection_refs",
            serde_json::json!([{"kind":"env","name":"OPENAI_API_KEY","secret":"parse-secret-sentinel"}]),
        ),
        (
            "model_policy",
            serde_json::json!({"kind":"exact","model":"profile-model"}),
        ),
    ] {
        let mut selected = base.clone();
        selected[field] = value;
        invalid.push(selected);
    }
    for selected in invalid {
        fs::write(&profile_file, serde_json::to_vec(&selected).unwrap()).unwrap();
        for many in [false, true] {
            let mut command = fixture.native_cli();
            if many {
                let tasks = fixture.0.join("invalid-tasks.json");
                fs::write(&tasks, serde_json::to_vec(&serde_json::json!([
                    {"id":"valid-first","prompt_file":"native-prompt","harness":"fake-a","request":{}},
                    {"id":"invalid-late","prompt_file":"native-prompt","harness":"omp",
                        "request":{"profile":selected,"model":"request-model"}}
                ])).unwrap()).unwrap();
                command
                    .args(["run-many", "test", "--tasks"])
                    .arg(tasks)
                    .args(["--concurrency", "2"]);
            } else {
                command
                    .args(["run", "test", "--harness", "omp", "--prompt-file"])
                    .arg(&prompt)
                    .arg("--profile")
                    .arg(&profile_file)
                    .args(["--model", "request-model"]);
            }
            let output = command
                .args(["--json", "--copy-limit-bytes", "10485760"])
                .output()
                .unwrap();
            assert!(!output.status.success(), "{selected}: {output:?}");
            assert!(output.stdout.is_empty());
            let failure: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert!(failure["error"]["kind"].is_string(), "{failure}");
            for secret in [
                "authorized-key-secret-sentinel",
                "parse-secret-sentinel",
                "config-secret-sentinel",
            ] {
                assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
            }
            assert_eq!(fixture.inventory(), before);
        }
    }
    for bytes in [
        vec![b'x'; 64 * 1024 + 1],
        b"{\"secret\":\"parse-secret-sentinel\"".to_vec(),
    ] {
        fs::write(&profile_file, bytes).unwrap();
        let output = fixture
            .native_cli()
            .args(["run", "test", "--harness", "omp", "--prompt-file"])
            .arg(&prompt)
            .arg("--profile")
            .arg(&profile_file)
            .arg("--json")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("parse-secret-sentinel"));
        assert_eq!(fixture.inventory(), before);
    }
    for directory in [
        "workspaces",
        "fork-staging",
        "artifact-staging",
        "artifacts",
    ] {
        let path = fixture.0.join("storage").join(directory);
        assert!(!path.exists() || fs::read_dir(path).unwrap().next().is_none());
    }
}

#[test]
fn registered_profile_digest_verification_is_strict_and_legacy_absence_is_compatible() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    for case in [
        "legacy",
        "missing-evidence",
        "missing-digest",
        "changed-digest",
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        let report = pending_workspace(&fixture);
        let path = artifact_container(&fixture, &report).join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
        db.execute_batch("DROP TRIGGER worker_attempt_immutable_evidence; DROP TRIGGER immutable_artifact_update;").unwrap();
        if matches!(case, "legacy" | "missing-evidence") {
            manifest.as_object_mut().unwrap().remove("profile");
            let bytes = serde_json::to_vec(&manifest).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&path, &bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
            db.execute(
                "UPDATE artifacts SET sha256=?1, size_bytes=?2 WHERE run_id=?3 AND kind='manifest'",
                rusqlite::params![
                    format!("{:x}", Sha256::digest(&bytes)),
                    bytes.len() as i64,
                    report["run_id"].as_str().unwrap()
                ],
            )
            .unwrap();
        }
        if matches!(case, "legacy" | "missing-digest") {
            db.execute("UPDATE worker_attempts SET profile_digest=NULL", [])
                .unwrap();
        }
        if case == "legacy" {
            // Historical runs predate immutable capture contexts and source receipts.
            db.execute_batch(
                "DROP TABLE run_environments; DROP TABLE environments; DROP TABLE run_launch_receipts; DROP TABLE prepared_source_evidence; DROP TABLE connector_deliveries; DROP TABLE run_targets; DROP TABLE run_observations; DROP TABLE run_capture_contexts; PRAGMA user_version=11;",
            )
            .unwrap();
        }
        if case == "changed-digest" {
            db.execute(
                "UPDATE worker_attempts SET profile_digest=?1",
                ["f".repeat(64)],
            )
            .unwrap();
        }
        drop(db);
        if case == "legacy" {
            let mut old_report = report.clone();
            old_report.as_object_mut().unwrap().remove("profile");
            let decoded: fabric_core::run::RunReport = serde_json::from_value(old_report).unwrap();
            assert!(decoded.profile.is_none());
        }
        let result = gc(&fixture, "--apply");
        assert_eq!(
            result["items"][0]["action"],
            if case == "legacy" {
                "reclaimed"
            } else {
                "refused"
            },
            "{case}: {result}"
        );
        if case != "legacy" {
            assert!(Path::new(report["workspace_path"].as_str().unwrap()).exists());
        }
    }
}

fn codex_fixture(fixture: &Fixture, version: &str) -> serde_json::Value {
    use std::os::unix::fs::PermissionsExt;
    let config = fixture.0.join("codex-config");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "# selected config\n").unwrap();
    fs::write(config.join("auth.json"), "opaque-auth-secret-sentinel").unwrap();
    let script = format!(
        r#"#!/bin/sh
set -eu
test -d "$HOME"
test -d "$XDG_CONFIG_HOME"
test -d "$XDG_DATA_HOME"
test -d "$XDG_STATE_HOME"
test -d "$XDG_CACHE_HOME"
test -z "${{UNSELECTED_SECRET-}}${{OPENAI_API_KEY-}}${{CODEX_ACCESS_TOKEN-}}${{HTTPS_PROXY-}}"
if [ "$1" = --version ]; then
    test -z "${{CODEX_API_KEY-}}"
    printf '%s\n' '{version}'
    exit 0
fi
test "$#" = 9
test "$1" = exec
test "$2" = --json
test "$3" = --ephemeral
test "$4" = --color
test "$5" = never
test "$6" = --sandbox
test "$7" = workspace-write
test "$8" = --
test "$9" = 'literal ; $(touch INJECTED)'
"#
    );
    let script = format!(
        "{script}\ntest \"$CODEX_HOME\" = '{}'\ntest \"$HOME\" != '{}'\ntest \"$CODEX_API_KEY\" = authorized-key-secret-sentinel\ntest -f .deps/ready\ntest -f tracked\ntest -z \"$(cat)\"\nprintf codex-change > tracked\nprintf '%s\\n' '{{\"type\":\"item.updated\",\"item\":{{\"id\":\"a\",\"type\":\"agent_message\",\"text\":\"partial\"}}}}' '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"a\",\"type\":\"agent_message\",\"text\":\"done\"}}}}' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":12,\"cached_input_tokens\":3,\"output_tokens\":4}}}}'\n",
        config.display(),
        fixture.0.display()
    );
    let binary = fixture.0.join("codex");
    fs::write(&binary, script).unwrap();
    fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).unwrap();
    serde_json::json!({
        "harness_id":"codex", "harness_version":"codex-cli 0.171.0",
        "native_config_refs":[{"kind":"native-config-dir","path":config}],
        "connection_refs":[{"kind":"native-auth","name":"native-config-dir"},{"kind":"env","name":"CODEX_API_KEY"}]
    })
}

fn codex_cli(fixture: &Fixture) -> Command {
    let mut command = fixture.cli();
    command
        .env("PATH", format!("{}:/usr/bin:/bin", fixture.0.display()))
        .env("CODEX_API_KEY", "authorized-key-secret-sentinel")
        .env("OPENAI_API_KEY", "unselected-openai-secret")
        .env("CODEX_HOME", fixture.0.join("hostile-config"))
        .env("CODEX_ACCESS_TOKEN", "unselected-token-secret")
        .env("HTTPS_PROXY", "unselected-proxy-secret");
    command
}

#[test]
fn codex_cli_mixed_batch_preserves_profile_artifacts_and_atomic_admission() {
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let profile = codex_fixture(&fixture, "codex-cli 0.171.0");
    fs::write(
        fixture.0.join("codex-prompt"),
        "literal ; $(touch INJECTED)",
    )
    .unwrap();
    fs::write(
        fixture.0.join("fake-prompt"),
        r#"{"writes":[{"path":"tracked","content":"fake-change"}]}"#,
    )
    .unwrap();
    let tasks_path = fixture.0.join("codex-tasks.json");
    let tasks = serde_json::json!([
        {"id":"fake","harness":"fake-b","prompt_file":"fake-prompt","request":{}},
        {"id":"codex","harness":"codex","prompt_file":"codex-prompt","request":{"profile":profile}}
    ]);
    fs::write(&tasks_path, serde_json::to_vec(&tasks).unwrap()).unwrap();
    let invoke = || {
        codex_cli(&fixture)
            .args(["run-many", "test", "--tasks"])
            .arg(&tasks_path)
            .args([
                "--concurrency",
                "2",
                "--json",
                "--copy-limit-bytes",
                "10485760",
            ])
            .output()
            .unwrap()
    };
    let output = invoke();
    let report = json(&output);
    assert_eq!(report["succeeded"], true);
    for (index, harness) in ["fake-b", "codex"].iter().enumerate() {
        let run = &report["tasks"][index]["result"];
        assert_eq!(run["harness"], *harness);
        assert_eq!(run["state"], "SUCCEEDED");
        assert_eq!(run["prepared_state_id"], base["state_id"]);
        assert_eq!(run["worker_attempts"][0]["harness_resolved"], *harness);
        assert_eq!(run["capture"]["truncated"], false);
        verified_manifest(&fixture, run);
        assert!(!Path::new(run["workspace_path"].as_str().unwrap()).exists());
        let restored = restored_workspace(&fixture, run);
        assert_eq!(
            fs::read_to_string(restored.join("tracked")).unwrap(),
            ["fake-change", "codex-change"][index]
        );
        if *harness == "codex" {
            assert_eq!(run["harness_version"], "codex-cli 0.171.0");
            assert_eq!(run["profile"]["portable"], false);
            assert_eq!(run["profile"]["digest"].as_str().unwrap().len(), 64);
            assert_eq!(
                run["profile"]["native_config_fingerprint"]
                    .as_str()
                    .unwrap()
                    .len(),
                64
            );
            assert_eq!(
                run["worker_attempts"][0]["profile_digest"],
                run["profile"]["digest"]
            );
            let events =
                fs::read_to_string(artifact_container(&fixture, run).join("events.jsonl")).unwrap();
            let events: Vec<serde_json::Value> = events
                .lines()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap()["event"].clone()
                })
                .collect();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["type"] == "output")
                    .count(),
                1
            );
            assert!(events.iter().any(|event| event["type"] == "usage"
                && event["input_tokens"] == 12
                && event["output_tokens"] == 4
                && event["cost_usd"].is_null()));
        }
        for file in fs::read_dir(artifact_container(&fixture, run)).unwrap() {
            let bytes = fs::read(file.unwrap().path()).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("secret-sentinel"));
        }
    }
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "original"
    );
    assert!(!repo.join("INJECTED").exists());
    let before = fixture.inventory();
    for mode in ["version", "auth", "missing-env", "model", "resume"] {
        let mut rejected = tasks.clone();
        if mode == "version" {
            codex_fixture(&fixture, "codex-cli 999.0.0");
        } else {
            codex_fixture(&fixture, "codex-cli 0.171.0");
        }
        match mode {
            "auth" => rejected[1]["request"]["profile"]["connection_refs"] = serde_json::json!([]),
            "model" => rejected[1]["request"]["model"] = serde_json::json!("unverified-model"),
            "resume" => rejected[1]["request"]["resume"] = serde_json::json!("prior-session"),
            _ => {}
        }
        fs::write(&tasks_path, serde_json::to_vec(&rejected).unwrap()).unwrap();
        let failure = if mode == "missing-env" {
            codex_cli(&fixture)
                .env_remove("CODEX_API_KEY")
                .args(["run-many", "test", "--tasks"])
                .arg(&tasks_path)
                .args(["--concurrency", "2", "--json"])
                .output()
                .unwrap()
        } else {
            invoke()
        };
        assert!(!failure.status.success(), "{mode}: {failure:?}");
        let error: serde_json::Value = serde_json::from_slice(&failure.stderr).unwrap();
        assert!(error["error"]["kind"].is_string());
        if mode == "version" {
            assert_eq!(error["error"]["kind"], "unsupported_version");
        }
        if mode == "model" || mode == "resume" {
            assert_eq!(error["error"]["kind"], "unsupported_capability");
        }
        assert_eq!(fixture.inventory(), before, "{mode}");
        assert!(!String::from_utf8_lossy(&failure.stderr).contains("secret"));
    }
}

fn api_request() -> serde_json::Value {
    serde_json::json!({
        "version":1, "op":"spawn_many", "project":"test", "concurrency":2,
        "default_harness":"fake-a",
        "tasks":[
            {"id":"implement","prompt":"{}", "request":{"model":"literal-api-model"}},
            {"id":"review","prompt":"{}", "harness":"fake-b", "request":null}
        ],
        "options":{"copy_limit_bytes":10485760, "timeout_seconds":10}
    })
}

fn api_command(fixture: &Fixture, request: &serde_json::Value) -> Command {
    let path = fixture.0.join("request.json");
    fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
    let mut command = fixture.cli();
    command.args(["api", "--request"]).arg(path).arg("--json");
    command
}

#[test]
fn api_control_surface_preserves_mixed_durable_identities_and_one_failed_response() {
    let fixture = Fixture::new();
    let (_, base) = fixture.setup();
    let mut request = api_request();
    for fail in [false, true] {
        request["tasks"][1]["prompt"] =
            serde_json::json!(serde_json::json!({"exit_code":if fail {7} else {0}}).to_string());
        let output = api_command(&fixture, &request).output().unwrap();
        assert_eq!(output.status.success(), !fail, "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["version"], 1);
        assert_eq!(response["op"], "spawn_many");
        assert_eq!(response["batch"]["succeeded"], !fail);
        let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        let batch_id = response["batch"]["batch_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(store.batch_succeeded(&batch_id).unwrap(), Some(!fail));
        let saved = store.batch_tasks(&batch_id).unwrap();
        for (index, task) in response["batch"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let run = &task["result"];
            let harness = ["fake-a", "fake-b"][index];
            assert_eq!(run["harness"], harness);
            assert_eq!(run["prepared_state_id"], base["state_id"]);
            assert_eq!(run["worker_attempts"][0]["harness_requested"], harness);
            assert_eq!(run["worker_attempts"][0]["harness_resolved"], harness);
            assert_eq!(
                run["state"],
                if fail && index == 1 {
                    "FAILED"
                } else {
                    "SUCCEEDED"
                }
            );
            verified_manifest(&fixture, run);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(saved[index].result.as_ref().unwrap())
                    .unwrap(),
                *run
            );
            assert!(!Path::new(&saved[index].prompt_file).exists());
            assert!(
                !Path::new(&saved[index].prompt_file)
                    .parent()
                    .unwrap()
                    .exists()
            );
        }
        assert_eq!(
            response["batch"]["tasks"][0]["result"]["worker_attempts"][0]["model_requested"],
            "literal-api-model"
        );
        assert!(
            response["batch"]["tasks"][1]["result"]["worker_attempts"][0]["model_requested"]
                .is_null()
        );
    }
}

#[test]
fn api_invalid_requests_reject_atomically_without_echoing_secrets() {
    let fixture = Fixture::new();
    fixture.setup();
    let before = fixture.inventory();
    let valid = api_request();
    let mut cases = Vec::new();
    for (key, value) in [
        ("version", serde_json::json!(2)),
        ("op", serde_json::json!("unsupported")),
        ("project", serde_json::json!(" ")),
        ("concurrency", serde_json::json!(0)),
        ("tasks", serde_json::json!([])),
        (
            "tasks",
            serde_json::json!(vec![valid["tasks"][0].clone(); 129]),
        ),
        ("credentials", serde_json::json!("raw-secret-sentinel")),
        ("options", serde_json::json!({"max_cost_usd":1})),
        ("options", serde_json::json!({"timeout_seconds":0})),
        ("options", serde_json::json!({"max_log_bytes":0})),
        ("options", serde_json::json!({"max_artifact_bytes":0})),
    ] {
        let mut request = valid.clone();
        request[key] = value;
        cases.push(request);
    }
    for task in [
        serde_json::json!({"id":"review","prompt":"{}","harness":"fake-b","request":{"model":"unsupported"}}),
        serde_json::json!({"id":"review","prompt":"{}","harness":"fake-b","request":{"resume":"prior"}}),
        serde_json::json!({"id":"review","prompt":"{}","harness":"unknown-harness"}),
        serde_json::json!({"id":"review","prompt":"{}","request":{"profile":{"harness_id":"fake-b","harness_version":"fabric-fake-b 1"}}}),
        serde_json::json!({"id":"review","prompt":"{}","request":{"profile":{"harness_id":"fake-a","harness_version":"fabric-fake-a 1","api_key":"raw-secret-sentinel"}}}),
        serde_json::json!({"id":"review","prompt":"{}","request":{"profile":{"harness_id":"fake-a","harness_version":"fabric-fake-a 1","native_config_refs":[{"kind":"native-home","path":"relative"}]}}}),
        serde_json::json!({"id":"review","prompt":"x".repeat(65537)}),
        serde_json::json!({"id":"implement","prompt":"{}"}),
    ] {
        let mut request = valid.clone();
        request["tasks"][1] = task;
        cases.push(request);
    }
    let mut oversized = valid.clone();
    oversized["tasks"][0]["prompt"] = serde_json::json!("raw-secret-sentinel".repeat(65536));
    cases.push(oversized);
    for request in cases {
        let output = api_command(&fixture, &request).output().unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(error["error"]["kind"].is_string());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("raw-secret-sentinel"));
        assert_eq!(fixture.inventory(), before);
    }
    let path = fixture.0.join("request.json");
    fs::write(&path, b"{\"raw-secret-sentinel\":").unwrap();
    let output = fixture
        .cli()
        .args(["api", "--request"])
        .arg(path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("raw-secret-sentinel"));
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn api_sigterm_cancels_owned_worker_seals_evidence_and_reclaims() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let request = serde_json::json!({
        "version":1, "op":"spawn_many", "project":"test", "concurrency":1,
        "default_harness":"fake",
        "tasks":[{"id":"blocking","prompt":r#"{"process_tree":{"ignore_child_term":true}}"#}],
        "options":{"copy_limit_bytes":10485760,"timeout_seconds":10}
    });
    let child = api_command(&fixture, &request)
        .arg("--fake-binary")
        .arg(env!("CARGO_BIN_EXE_fabric-fake-harness"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let tree = wait_for_tree(&fixture);
    let parent_pid: i32 = fs::read_to_string(tree.join("parent.heartbeat"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .strip_prefix("pid=")
        .unwrap()
        .parse()
        .unwrap();
    let db = rusqlite::Connection::open_with_flags(
        fixture.0.join("storage/fabric.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let prompt: String = db
        .query_row("SELECT prompt_file FROM batch_tasks", [], |row| row.get(0))
        .unwrap();
    let prompt = PathBuf::from(prompt);
    assert_eq!(
        fs::metadata(&prompt).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(prompt.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    success(
        &Command::new("/bin/kill")
            .args(["-TERM", &child.id().to_string()])
            .output()
            .unwrap(),
    );
    let output = child.wait_with_output().unwrap();
    // A regressed signal handler must not leave this blocking fixture running.
    if output.stdout.is_empty() {
        unsafe {
            libc::kill(-parent_pid, libc::SIGKILL);
        }
        let _ = fs::remove_dir_all(prompt.parent().unwrap());
    }
    assert!(!output.status.success(), "{output:?}");
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!("SIGTERM must return one durable API batch response: {output:?}")
    });
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(response["version"], 1);
    assert_eq!(response["op"], "spawn_many");
    assert_eq!(response["batch"]["succeeded"], false);
    let report = &response["batch"]["tasks"][0]["result"];
    assert_eq!(report["state"], "CANCELLED");
    assert_eq!(report["error_kind"], "cancelled");
    assert_eq!(
        report["worker_attempts"][0]["termination_reason"],
        "cancelled"
    );
    assert_eq!(report["exit_signal"], 15);
    assert_eq!(
        verified_manifest(&fixture, report)["stop_reason"],
        "cancelled"
    );
    assert_tree_stopped(&fixture, report);
    let parent = Command::new("/bin/ps")
        .args(["-p", &parent_pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(parent.stdout).unwrap().trim().is_empty(),
        "owned child was not reaped"
    );
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let batch_id = response["batch"]["batch_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(store.batch_succeeded(&batch_id).unwrap(), Some(false));
    let task = store.batch_tasks(&batch_id).unwrap().pop().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(task.result.as_ref().unwrap()).unwrap(),
        *report
    );
    let run = store.run(&task.run_id).unwrap().unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Cancelled);
    assert_eq!(run.error_kind.as_deref(), Some("cancelled"));
    assert_eq!(
        store.workspace(&run.workspace_id).unwrap().unwrap().state,
        fabric_domain::WorkspaceState::Reclaimed
    );
    assert!(!prompt.parent().unwrap().exists());
}

fn environment_command(
    fixture: &Fixture,
    operation: &str,
    id: &str,
    owner: &str,
    target: &str,
    extra: &[&str],
) -> Output {
    fixture
        .cli()
        .args([
            "environments",
            operation,
            id,
            "--owner",
            owner,
            "--target",
            target,
            "--json",
        ])
        .args(extra)
        .output()
        .unwrap()
}

fn assert_environment_error(output: &Output, kind: &str) {
    assert!(!output.status.success(), "{output:?}");
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], kind, "{output:?}");
}

#[test]
fn environment_persists_across_cli_and_target_recreation_without_capturing_home() {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    let (_, prepared) = fixture.setup();
    fixture.target("env-local", "environment");
    let first = json(&environment_command(
        &fixture,
        "ensure",
        "durable-home",
        "alice",
        "env-local",
        &[],
    ));
    let home = PathBuf::from(first["home"].as_str().unwrap());
    let identity = fs::metadata(&home).unwrap();
    let observed = fixture.0.join("environment-observed.jsonl");
    let binary = fixture.script_fake(&format!(r#"exec /usr/bin/python3 - <<'PY'
import json, os, sqlite3
home = os.environ['HOME']
assert home == {home:?}
assert os.environ['XDG_CONFIG_HOME'] == os.path.join(home, '.config')
assert os.environ['XDG_DATA_HOME'] == os.path.join(home, '.local/share')
assert os.environ['XDG_STATE_HOME'] == os.path.join(home, '.local/state')
assert os.environ['XDG_CACHE_HOME'] == os.path.join(home, '.cache')
assert 'UNSELECTED_SECRET' not in os.environ
assert not os.path.exists('only-this-run')
with open('tracked') as f: assert f.read() == 'original'
with open('.deps/ready') as f: assert f.read() == 'ready'
db = sqlite3.connect({database:?})
receipt = json.loads(db.execute('SELECT receipt_json FROM run_launch_receipts WHERE json_extract(receipt_json, "$.workspace_path")=?', (os.getcwd(),)).fetchone()[0])
with open({snapshot:?}) as f: assert receipt == json.load(f)
assert receipt['actual_sha256'] == receipt['source']['prepared_sha256']
assert receipt['source_commit_sha'] == {commit:?}
marker = os.path.join(home, '.config', 'private-marker')
existed = os.path.exists(marker)
if existed:
    with open(marker) as f: assert f.read() == 'private-home-secret-sentinel'
else:
    with open(marker, 'w') as f: f.write('private-home-secret-sentinel')
with open('only-this-run', 'w') as f: f.write(receipt['run_id'])
with open({observed:?}, 'a') as f: f.write(json.dumps(dict(receipt=receipt, home=home, existed=existed)) + '\n')
PY"#,
        home = home.to_str().unwrap(),
        database = fixture.0.join("storage/fabric.db").to_str().unwrap(),
        snapshot = fixture.0.join("prestart-source.json").to_str().unwrap(),
        commit = prepared["source_commit_sha"].as_str().unwrap(),
        observed = observed.to_str().unwrap()));
    let prompt = fixture.0.join("env-prompt.json");
    fs::write(&prompt, "{}").unwrap();
    let mut reports = Vec::new();
    for _ in 0..2 {
        let report = json(
            &fixture
                .cli()
                .args(["run", "test", "--harness", "fake", "--fake-binary"])
                .arg(&binary)
                .arg("--prompt-file")
                .arg(&prompt)
                .args([
                    "--placement",
                    "env-local",
                    "--environment",
                    "durable-home",
                    "--environment-owner",
                    "alice",
                    "--copy-limit-bytes",
                    "10485760",
                    "--json",
                ])
                .output()
                .unwrap(),
        );
        assert_eq!(report["state"], "SUCCEEDED");
        assert_eq!(report["environment_id"], "durable-home");
        verified_manifest(&fixture, &report);
        let captured = restored_workspace(&fixture, &report);
        assert_eq!(
            fs::read(captured.join("only-this-run")).unwrap(),
            report["run_id"].as_str().unwrap().as_bytes()
        );
        assert!(!captured.join(".config/private-marker").exists());
        let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        let run = report["run_id"].as_str().unwrap().parse().unwrap();
        for artifact in store.artifacts(&run).unwrap() {
            let bytes = fs::read(fixture.0.join("storage").join(artifact.relative_path)).unwrap();
            assert!(
                !bytes
                    .windows(b"private-home-secret-sentinel".len())
                    .any(|bytes| bytes == b"private-home-secret-sentinel")
            );
        }
        assert_eq!(
            fs::read(home.join(".config/private-marker")).unwrap(),
            b"private-home-secret-sentinel"
        );
        let reopened = json(&environment_command(
            &fixture,
            "ensure",
            "durable-home",
            "alice",
            "env-local",
            &[],
        ));
        assert_eq!(reopened["environment_id"], first["environment_id"]);
        assert_eq!(reopened["home"], first["home"]);
        let metadata = fs::metadata(&home).unwrap();
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (identity.dev(), identity.ino())
        );
        reports.push(report);
    }
    assert_ne!(reports[0]["run_id"], reports[1]["run_id"]);
    assert_ne!(reports[0]["workspace_id"], reports[1]["workspace_id"]);
    assert_ne!(reports[0]["workspace_path"], reports[1]["workspace_path"]);
    assert_ne!(reports[0]["run_id"], reports[0]["workspace_id"]);
    let observations: Vec<serde_json::Value> = String::from_utf8(fs::read(observed).unwrap())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(observations[0]["existed"], false);
    assert_eq!(observations[1]["existed"], true);
    for (observation, report) in observations.iter().zip(&reports) {
        assert_eq!(observation["receipt"]["run_id"], report["run_id"]);
        assert_eq!(
            observation["receipt"]["workspace_path"],
            report["workspace_path"]
        );
        assert_eq!(
            observation["receipt"]["prepared_state_id"],
            prepared["state_id"]
        );
        let retained = json(
            &fixture
                .cli()
                .args(["runs", "show", report["run_id"].as_str().unwrap(), "--json"])
                .output()
                .unwrap(),
        );
        assert_eq!(retained["environment_id"], "durable-home");
        assert_eq!(retained["launch_receipt"], observation["receipt"]);
    }
    let inspected = json(&environment_command(
        &fixture,
        "inspect",
        "durable-home",
        "alice",
        "env-local",
        &[],
    ));
    assert_eq!(inspected["runtime"], "available");
    assert_eq!(inspected["runs"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read(Path::new(prepared["prepared_path"].as_str().unwrap()).join("tracked")).unwrap(),
        b"original"
    );
    assert!(
        !Path::new(prepared["prepared_path"].as_str().unwrap())
            .join("only-this-run")
            .exists()
    );
    let destroyed = json(&environment_command(
        &fixture,
        "destroy",
        "durable-home",
        "alice",
        "env-local",
        &["--confirm", "durable-home"],
    ));
    assert_eq!(destroyed["desired"], "destroyed");
    assert_eq!(destroyed["runtime"], "absent");
    assert!(!home.parent().unwrap().exists());
    let reconciled = json(&environment_command(
        &fixture,
        "inspect",
        "durable-home",
        "alice",
        "env-local",
        &[],
    ));
    assert_eq!(reconciled["runtime"], "absent");
    assert_environment_error(
        &environment_command(
            &fixture,
            "ensure",
            "durable-home",
            "alice",
            "env-local",
            &[],
        ),
        "storage",
    );
}

#[test]
fn environment_destroy_requires_matching_owner_target_confirmation_and_native_lease() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("env-local", "environment");
    fixture.target("other-target", "environment");
    let report = json(&environment_command(
        &fixture,
        "ensure",
        "owned-home",
        "alice",
        "env-local",
        &[],
    ));
    let home = PathBuf::from(report["home"].as_str().unwrap());
    fs::write(home.join("private-marker"), "retain").unwrap();
    for (owner, target, confirmation) in [
        ("bob", "env-local", "owned-home"),
        ("alice", "other-target", "owned-home"),
        ("alice", "env-local", "yes"),
    ] {
        assert_environment_error(
            &environment_command(
                &fixture,
                "destroy",
                "owned-home",
                owner,
                target,
                &["--confirm", confirmation],
            ),
            "authorization_required",
        );
        assert_eq!(fs::read(home.join("private-marker")).unwrap(), b"retain");
    }
    let prompt = fixture.0.join("env-prompt.json");
    fs::write(&prompt, "{}").unwrap();
    let run = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--prompt-file"])
            .arg(prompt)
            .args([
                "--placement",
                "env-local",
                "--environment",
                "owned-home",
                "--environment-owner",
                "alice",
                "--copy-limit-bytes",
                "10485760",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let token = run["run_id"].as_str().unwrap();
    let lease = fixture.0.join("storage/leases").join(token);
    let retained = lease.with_extension("original");
    fs::rename(&lease, &retained).unwrap();
    fs::copy(&retained, &lease).unwrap();
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute(
        "UPDATE worker_leases SET expires_at=0 WHERE token=?1",
        [token],
    )
    .unwrap();
    assert_environment_error(
        &environment_command(
            &fixture,
            "destroy",
            "owned-home",
            "alice",
            "env-local",
            &["--confirm", "owned-home"],
        ),
        "storage",
    );
    assert_eq!(fs::read(home.join("private-marker")).unwrap(), b"retain");
    fs::remove_file(&lease).unwrap();
    fs::rename(retained, lease).unwrap();
    let destroyed = json(&environment_command(
        &fixture,
        "destroy",
        "owned-home",
        "alice",
        "env-local",
        &["--confirm", "owned-home"],
    ));
    assert_eq!(destroyed["runtime"], "absent");
}

#[test]
fn environment_reconciliation_and_destroy_refuse_symlink_unowned_and_replaced_resources() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for fault in [
        "symlink",
        "config-symlink",
        "unowned",
        "replaced",
        "replaced:.config",
        "replaced:.cache",
        "replaced:.local",
        "replaced:.local/share",
        "replaced:.local/state",
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        fixture.target("env-local", "environment");
        let report = json(&environment_command(
            &fixture,
            "ensure",
            "guarded-home",
            "alice",
            "env-local",
            &[],
        ));
        let home = PathBuf::from(report["home"].as_str().unwrap());
        let protected = fixture.0.join("protected");
        fs::create_dir(&protected).unwrap();
        fs::write(protected.join("sentinel"), "external").unwrap();
        match fault {
            "symlink" => {
                fs::rename(&home, home.with_extension("original")).unwrap();
                symlink(&protected, &home).unwrap();
            }
            "config-symlink" => {
                fs::rename(home.join(".config"), home.join(".config-original")).unwrap();
                symlink(&protected, home.join(".config")).unwrap();
            }
            "unowned" => fs::remove_file(home.parent().unwrap().join(".fabric-owner")).unwrap(),
            "replaced" => {
                fs::rename(&home, home.with_extension("original")).unwrap();
                fs::create_dir(&home).unwrap();
                fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            }
            fault if fault.starts_with("replaced:") => {
                let relative = fault.strip_prefix("replaced:").unwrap();
                let directory = home.join(relative);
                fs::rename(&directory, fixture.0.join("saved-config-root")).unwrap();
                fs::create_dir(&directory).unwrap();
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
                if relative == ".local" {
                    for name in ["share", "state"] {
                        let child = directory.join(name);
                        fs::create_dir(&child).unwrap();
                        fs::set_permissions(child, fs::Permissions::from_mode(0o700)).unwrap();
                    }
                }
            }
            _ => unreachable!(),
        }
        let inspected = json(&environment_command(
            &fixture,
            "inspect",
            "guarded-home",
            "alice",
            "env-local",
            &[],
        ));
        assert_eq!(inspected["runtime"], "unknown", "{fault}");
        assert!(inspected["home"].is_null());
        assert_environment_error(
            &environment_command(
                &fixture,
                "destroy",
                "guarded-home",
                "alice",
                "env-local",
                &["--confirm", "guarded-home"],
            ),
            "storage",
        );
        assert_eq!(fs::read(protected.join("sentinel")).unwrap(), b"external");
        assert!(fs::symlink_metadata(&home).is_ok());
    }
}

#[test]
fn environment_legacy_protocol_and_unsupported_capabilities_never_allocate_or_fallback() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("legacy", "ok");
    fixture.target("env-local", "environment");
    fixture.target("invalid-env", "environment-unknown");
    let before = fixture.inventory();
    assert_environment_error(
        &environment_command(&fixture, "ensure", "no-legacy-home", "alice", "legacy", &[]),
        "unsupported_capability",
    );
    assert_environment_error(
        &environment_command(
            &fixture,
            "ensure",
            "no-pty-home",
            "alice",
            "env-local",
            &["--interactive-exec-pty"],
        ),
        "unsupported_capability",
    );
    assert!(
        !environment_command(
            &fixture,
            "ensure",
            "invalid-home",
            "alice",
            "invalid-env",
            &[]
        )
        .status
        .success()
    );
    assert_eq!(fixture.inventory(), before);
    for id in ["no-legacy-home", "no-pty-home", "invalid-home"] {
        assert!(!fixture.0.join("storage/environments").join(id).exists());
    }
    json(&environment_command(
        &fixture,
        "ensure",
        "valid-home",
        "alice",
        "env-local",
        &[],
    ));
    for operation in ["start", "stop"] {
        assert_environment_error(
            &environment_command(&fixture, operation, "valid-home", "alice", "env-local", &[]),
            "unsupported_capability",
        );
    }
    let prompt = fixture.0.join("env-prompt.json");
    fs::write(&prompt, "{}").unwrap();
    for (target, pty) in [("legacy", false), ("env-local", true)] {
        let mut command = fixture.cli();
        command
            .args(["run", "test", "--harness", "fake", "--prompt-file"])
            .arg(&prompt)
            .args([
                "--placement",
                target,
                "--environment",
                "valid-home",
                "--environment-owner",
                "alice",
                "--copy-limit-bytes",
                "10485760",
                "--json",
            ]);
        if pty {
            command.arg("--interactive-exec-pty");
        }
        assert_environment_error(&command.output().unwrap(), "unsupported_capability");
        assert_eq!(fixture.inventory(), before);
    }
    let legacy = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--prompt-file"])
            .arg(&prompt)
            .args([
                "--placement",
                "legacy",
                "--copy-limit-bytes",
                "10485760",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(legacy["state"], "SUCCEEDED");
    assert!(legacy["environment_id"].is_null());
    verified_manifest(&fixture, &legacy);
    let log = fs::read_to_string(fixture.0.join("target-legacy.jsonl")).unwrap();
    assert!(
        log.lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .all(|request| !request["method"]
                .as_str()
                .unwrap()
                .starts_with("target.environment."))
    );
}

#[test]
fn environment_cancellation_and_expired_active_lease_retain_private_home() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("local", "environment");
    let ensured = json(&environment_command(
        &fixture,
        "ensure",
        "cancel-home",
        "alice",
        "local",
        &[],
    ));
    let home = PathBuf::from(ensured["home"].as_str().unwrap());
    let binary = fixture.script_fake(&format!(
        "printf cancellation-private-marker > \"$HOME/private-marker\"\nexec {:?} \"$@\"",
        env!("CARGO_BIN_EXE_fabric-fake-harness")
    ));
    let prompt = fixture.0.join("env-cancel.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let options = fabric_core::run::RunOptions {
        environment: Some(fabric_domain::EnvironmentSelection {
            environment_id: "cancel-home".parse().unwrap(),
            owner: "alice".into(),
            interactive_exec_pty: false,
        }),
        copy_limit_bytes: 10485760,
        limits: fabric_runner::ExecutionLimits {
            timeout: std::time::Duration::from_secs(30),
            term_grace: std::time::Duration::from_millis(100),
        },
        ..fixture.options()
    };
    let harness = fabric_runner::FakeHarness::new(binary).unwrap();
    let report = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            fabric_core::run::foreground(
                &fixture.0.join("storage"),
                "test",
                &prompt,
                &harness,
                &options,
            )
        });
        wait_for_tree(&fixture);
        let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
        db.execute("UPDATE worker_leases SET expires_at=0", [])
            .unwrap();
        let denied = environment_command(
            &fixture,
            "destroy",
            "cancel-home",
            "alice",
            "local",
            &["--confirm", "cancel-home"],
        );
        options.cancellation.cancel();
        let result = worker.join().unwrap().unwrap();
        assert_environment_error(&denied, "storage");
        result
    });
    assert_eq!(report.state, fabric_domain::RunState::Cancelled);
    assert_eq!(report.error_kind.as_deref(), Some("cancelled"));
    let report = serde_json::to_value(report).unwrap();
    assert_eq!(
        verified_manifest(&fixture, &report)["stop_reason"],
        "cancelled"
    );
    assert_tree_stopped(&fixture, &report);
    assert_eq!(
        fs::read(home.join("private-marker")).unwrap(),
        b"cancellation-private-marker"
    );
    let inspected = json(&environment_command(
        &fixture,
        "inspect",
        "cancel-home",
        "alice",
        "local",
        &[],
    ));
    assert_eq!(inspected["runtime"], "available");
    assert_eq!(inspected["runs"][0], report["run_id"]);
    let destroyed = json(&environment_command(
        &fixture,
        "destroy",
        "cancel-home",
        "alice",
        "local",
        &["--confirm", "cancel-home"],
    ));
    assert_eq!(destroyed["runtime"], "absent");
}

#[test]
fn environment_ensure_never_adopts_a_preexisting_caller_named_directory() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("env-local", "environment");
    let container = fixture.0.join("storage/environments/unowned-home");
    let tree = container.join("tree");
    fs::create_dir_all(&tree).unwrap();
    for directory in [
        container.parent().unwrap(),
        container.as_path(),
        tree.as_path(),
    ] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(tree.join("sentinel"), "not-owned").unwrap();
    assert_environment_error(
        &environment_command(
            &fixture,
            "ensure",
            "unowned-home",
            "alice",
            "env-local",
            &[],
        ),
        "storage",
    );
    let report = json(&environment_command(
        &fixture,
        "inspect",
        "unowned-home",
        "alice",
        "env-local",
        &[],
    ));
    assert_eq!(report["runtime"], "unknown");
    assert!(report["home"].is_null());
    assert_environment_error(
        &environment_command(
            &fixture,
            "destroy",
            "unowned-home",
            "alice",
            "env-local",
            &["--confirm", "unowned-home"],
        ),
        "storage",
    );
    assert_eq!(fs::read(tree.join("sentinel")).unwrap(), b"not-owned");
}

#[test]
fn environment_rejects_native_auth_context_without_reading_or_exporting_it() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("env-local", "environment");
    let ensured = json(&environment_command(
        &fixture,
        "ensure",
        "private-home",
        "alice",
        "env-local",
        &[],
    ));
    let home = PathBuf::from(ensured["home"].as_str().unwrap());
    fs::write(home.join("private-marker"), b"keep-private").unwrap();
    let profile_file = fixture.0.join("profile.json");
    let prompt = fixture.0.join("environment-prompt.json");
    fs::write(&prompt, b"{}").unwrap();
    for profile in [
        serde_json::json!({
            "harness_id":"fake", "harness_version":"fabric-fake-harness 1",
            "native_config_refs":[{"kind":"native-home","path":fixture.0.join("must-not-be-read")}]
        }),
        serde_json::json!({
            "harness_id":"fake", "harness_version":"fabric-fake-harness 1",
            "connection_refs":[{"kind":"native-auth","name":"must-not-be-exported"}]
        }),
    ] {
        fs::write(&profile_file, serde_json::to_vec(&profile).unwrap()).unwrap();
        let output = fixture
            .cli()
            .args([
                "run",
                "test",
                "--harness",
                "fake",
                "--placement",
                "env-local",
                "--environment",
                "private-home",
                "--environment-owner",
                "alice",
                "--json",
            ])
            .arg("--profile")
            .arg(&profile_file)
            .arg("--prompt-file")
            .arg(&prompt)
            .output()
            .unwrap();
        assert_environment_error(&output, "unsupported_capability");
        assert_eq!(
            fs::read(home.join("private-marker")).unwrap(),
            b"keep-private"
        );
        let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
        for table in ["runs", "workspaces", "run_launch_receipts"] {
            let count: i64 = db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }
}

#[test]
fn environment_destroy_allows_recovered_needs_attention_with_verified_inactive_lease() {
    let fixture = Fixture::new();
    fixture.setup();
    fixture.target("local", "environment-crash");
    let ensured = json(&environment_command(
        &fixture,
        "ensure",
        "interrupted-home",
        "alice",
        "local",
        &[],
    ));
    let home = PathBuf::from(ensured["home"].as_str().unwrap());
    fs::write(fixture.0.join("await-worker"), "").unwrap();
    let binary = fixture.script_fake(&format!(
        "printf private > \"$HOME/private-marker\"\ntouch {:?}\nexec /bin/sleep 30",
        fixture.0.join("worker-ready")
    ));
    let prompt = fixture.0.join("interrupted-prompt.json");
    fs::write(&prompt, "{}").unwrap();
    let output = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(binary)
        .arg("--prompt-file")
        .arg(prompt)
        .args([
            "--environment",
            "interrupted-home",
            "--environment-owner",
            "alice",
            "--timeout-seconds",
            "1",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let runs = json(
        &fixture
            .cli()
            .args(["runs", "list", "--json"])
            .output()
            .unwrap(),
    );
    let run_id = runs[0]["run_id"].as_str().unwrap();
    let before = json(
        &fixture
            .cli()
            .args(["runs", "show", run_id, "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(before["run"]["state"], "NEEDS_ATTENTION");
    assert_eq!(fs::read(home.join("private-marker")).unwrap(), b"private");
    let recovered = json(
        &fixture
            .cli()
            .args(["recover", run_id, "--apply", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(recovered["action"], "reclaimed");
    let destroyed = json(&environment_command(
        &fixture,
        "destroy",
        "interrupted-home",
        "alice",
        "local",
        &["--confirm", "interrupted-home"],
    ));
    assert_eq!(destroyed["runtime"], "absent");
    assert!(!home.exists());
    let after = json(
        &fixture
            .cli()
            .args(["runs", "show", run_id, "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(after["run"]["state"], "NEEDS_ATTENTION");
    assert_eq!(after["run"]["capture"], before["run"]["capture"]);
    assert_sealed_artifacts(&fixture, run_id);
}
