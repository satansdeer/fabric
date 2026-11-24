//! Public CLI -> installed remote.v1 target -> real generic agent, provider-free.
use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

impl Fixture {
    fn remote_target(&self) -> PathBuf {
        let root = self.0.join("remote-retained");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let script = self.0.join("remote-target.py");
        fs::write(&script, include_bytes!("../remote_target.py")).unwrap();
        let config = self.0.join("remote-target-config.json");
        fs::write(&config, serde_json::to_vec(&serde_json::json!({
            "root":root,"agent":env!("CARGO_BIN_EXE_fabric-remote-agent")
        })).unwrap()).unwrap();
        let manifest = self.0.join("remote-target.json");
        fs::write(&manifest, serde_json::to_vec(&serde_json::json!({
            "id":"remote-proof","version":"0.1.0","kind":"target",
            "protocol":{"min":1,"max":1},"executable":"/usr/bin/python3",
            "args":[script,config],"capabilities":["target.execute"]
        })).unwrap()).unwrap();
        success(&self.cli().args(["plugins","install"]).arg(manifest).output().unwrap());
        let ensured = json(&environment_command(self,"ensure","retained-home","alice","remote-proof",&[]));
        assert_eq!(ensured["runtime"],"available");
        root
    }

    fn remote_command(&self, exit_code: i32) -> Command {
        let prompt = self.0.join("remote-prompt.json");
        fs::write(&prompt,serde_json::to_vec(&serde_json::json!({
            "writes":[{"path":"tracked","content":"retained remote bytes\n"},
                      {"path":"remote.bin","content":"artifact retained\n"}],
            "events":[{"type":"fixture","value":42}],"exit_code":exit_code,
            "absent_environment":["UNSELECTED_SECRET"]
        })).unwrap()).unwrap();
        let mut command = self.cli();
        command.args(["run","test","--harness","fake","--placement","remote-proof",
            "--environment","retained-home","--environment-owner","alice","--prompt-file"])
            .arg(prompt).args(["--copy-limit-bytes","10485760","--json"]);
        command
    }

    fn only_run(&self) -> fabric_store::Run {
        let db = rusqlite::Connection::open_with_flags(self.0.join("storage/fabric.db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let id: String = db.query_row("SELECT run_id FROM runs",[],|row|row.get(0)).unwrap();
        drop(db);
        fabric_store::Store::open_existing_read_only(self.0.join("storage/fabric.db")).unwrap()
            .run(&id.parse().unwrap()).unwrap().unwrap()
    }

    fn crash_after_remote_completion(&self, remote: &Path, exit_code: i32) -> fabric_store::Run {
        fs::write(remote.join("pause-export"),b"test-only crash gate").unwrap();
        let mut controller = self.remote_command(exit_code).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline = Instant::now()+Duration::from_secs(10);
        while !remote.join("export-ready").exists() {
            assert!(controller.try_wait().unwrap().is_none(),"controller failed before persisted result");
            assert!(Instant::now()<deadline,"controller never reached completed-result export boundary");
            std::thread::sleep(Duration::from_millis(10));
        }
        controller.kill().unwrap();
        controller.wait().unwrap();
        while !remote.join("export-closed").exists() {
            assert!(Instant::now()<deadline,"original transport/agent not reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::remove_file(remote.join("pause-export")).unwrap();
        let run=self.only_run();
        assert!(run.capture.is_none());
        assert!(run.artifact_manifest_id.is_none());
        let retained=remote.join("work").join(run.run_id.as_str());
        let result:serde_json::Value=serde_json::from_slice(&fs::read(retained.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["exit_code"],exit_code);
        assert_eq!(fs::read(retained.join("tree/tracked")).unwrap(),b"retained remote bytes\n");
        run
    }
}

fn snapshot(root:&Path)->BTreeMap<PathBuf,Vec<u8>> {
    fn visit(base:&Path,path:&Path,files:&mut BTreeMap<PathBuf,Vec<u8>>) {
        for item in fs::read_dir(path).unwrap() {
            let path=item.unwrap().path();
            // SQLite's transient reader lock table is not durable run evidence.
            if path.file_name().unwrap().to_string_lossy().ends_with("-shm") {continue;}
            let meta=fs::symlink_metadata(&path).unwrap();
            let key=path.strip_prefix(base).unwrap().to_owned();
            if meta.is_dir() {files.insert(key,b"directory".to_vec());visit(base,&path,files);}
            else if meta.file_type().is_symlink() {files.insert(key,format!("link:{:?}",fs::read_link(path).unwrap()).into_bytes());}
            else {files.insert(key,fs::read(path).unwrap());}
        }
    }
    let mut files=BTreeMap::new();visit(root,root,&mut files);files
}

#[test]
fn remote_completed_crash_inspect_is_read_only_and_apply_seals_retained_bytes_without_rerun() {
    for exit_code in [0,7] {
        let fixture=Fixture::new();fixture.setup();let remote=fixture.remote_target();
        let before=fixture.crash_after_remote_completion(&remote,exit_code);
        let retained=remote.join("work").join(before.run_id.as_str());
        let result:serde_json::Value=serde_json::from_slice(&fs::read(retained.join("result.json")).unwrap()).unwrap();
        let logs:[Vec<u8>;3]=["stdout.log","stderr.log","events.jsonl"].map(|name|fs::read(retained.join(name)).unwrap());
        let durable=snapshot(&fixture.0.join("storage"));let evidence=snapshot(&remote.join("work"));
        let home=snapshot(&remote.join("home"));
        let report=json(&fixture.cli().args(["recover",before.run_id.as_str(),"--json"]).output().unwrap());
        assert_eq!(report["action"],"would_recover","{report}");
        assert_eq!(snapshot(&fixture.0.join("storage")),durable);
        assert_eq!(snapshot(&remote.join("work")),evidence);
        assert_eq!(snapshot(&remote.join("home")),home);
        let applied=json(&fixture.cli().args(["recover",before.run_id.as_str(),"--apply","--json"]).output().unwrap());
        assert_eq!(applied["action"],"reclaimed","{applied}");
        let after=fixture.only_run();
        assert_eq!(after.state,fabric_domain::RunState::NeedsAttention);
        assert_eq!(after.exit_code,Some(exit_code));assert_eq!(after.exit_signal,None);
        assert_eq!(serde_json::to_value(after.capture.as_ref().unwrap()).unwrap(),result["capture"]);
        assert!(after.artifact_manifest_id.is_some());assert!(after.error_kind.is_some());
        let artifacts=fixture.0.join("storage/artifacts").join(format!("a-{}",before.run_id));
        for (name,bytes) in ["stdout.log","stderr.log","events.jsonl"].into_iter().zip(logs) {
            assert_eq!(fs::read(artifacts.join(name)).unwrap(),bytes);
        }
        assert!(!retained.exists(),"reclaim follows sealed metadata and valid authority");
        let requests=fs::read_to_string(remote.join("requests.jsonl")).unwrap();
        let operations:Vec<_>=requests.lines().map(|line|serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|v|v["method"]=="target.remote.invoke").map(|v|v["params"]["operation"].as_str().unwrap().to_owned()).collect();
        assert_eq!(operations.iter().filter(|op|op.as_str()=="worker.start").count(),1);
        assert!(operations.iter().rposition(|op|op=="workspace.export").unwrap()<operations.iter().position(|op|op=="workspace.reclaim").unwrap());
    }
}

#[test]
fn invalid_retained_exports_refuse_without_adopting_partial_capture_or_reclaiming() {
    for mode in ["hash","size","binding","missing","capture_hash","result_binding"] {
        let fixture=Fixture::new();fixture.setup();let remote=fixture.remote_target();
        let run=fixture.crash_after_remote_completion(&remote,7);
        let retained=remote.join("work").join(run.run_id.as_str());
        match mode {
            "missing"=>fs::remove_file(retained.join("stderr.log")).unwrap(),
            "capture_hash"|"result_binding"=>{
                let path=retained.join("result.json");
                let mut value:serde_json::Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                if mode=="capture_hash" {value["capture"]["stdout"]["sha256"]=serde_json::json!("0".repeat(64));}
                else {value["resource"]["instance"]=serde_json::json!("foreign");}
                fs::write(path,serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _=>fs::write(remote.join("corrupt-export"),mode).unwrap(),
        }
        let durable=snapshot(&fixture.0.join("storage"));let evidence=snapshot(&remote.join("work"));
        let inspect=json(&fixture.cli().args(["recover",run.run_id.as_str(),"--json"]).output().unwrap());
        assert_eq!(inspect["action"],"retained","{mode}: {inspect}");
        assert_eq!(snapshot(&fixture.0.join("storage")),durable);
        assert_eq!(snapshot(&remote.join("work")),evidence);
        let apply=json(&fixture.cli().args(["recover",run.run_id.as_str(),"--apply","--json"]).output().unwrap());
        assert_eq!(apply["action"],"retained","{mode}: {apply}");
        let after=fixture.only_run();assert!(after.capture.is_none());assert!(after.artifact_manifest_id.is_none());
        assert_eq!(snapshot(&remote.join("work")),evidence);
    }
}

#[test]
fn remote_download_then_local_artifact_cap_is_not_a_target_protocol_failure() {
    for target_failure in [false,true] {
        let fixture=Fixture::new();fixture.setup();let remote=fixture.remote_target();
        if target_failure {fs::write(remote.join("fail-export"),b"test-only target failure").unwrap();}
        let output=fixture.remote_command(0).args(["--max-artifact-bytes","32"]).output().unwrap();
        assert!(!output.status.success(),"{output:?}");
        let run=fixture.only_run();
        assert_eq!(run.state,fabric_domain::RunState::NeedsAttention);
        assert_eq!(run.error_kind.as_deref(),Some(if target_failure {"target_protocol"} else {"artifact_capture"}));
        assert_eq!(run.exit_code,Some(0));assert!(run.artifact_manifest_id.is_none());
        assert_eq!(run.capture.is_some(),!target_failure);
        let store=fabric_store::Store::open_existing_read_only(fixture.0.join("storage/fabric.db")).unwrap();
        assert!(store.artifacts(&run.run_id).unwrap().is_empty());
        if !target_failure {
            let workspace=store.workspace(&run.workspace_id).unwrap().unwrap();
            let tree=Path::new(&workspace.managed_path);
            assert_eq!(fs::read(tree.join("tracked")).unwrap(),b"retained remote bytes\n");
            assert!(tree.parent().unwrap().join("stdout.log").is_file());
        }
    }
}
