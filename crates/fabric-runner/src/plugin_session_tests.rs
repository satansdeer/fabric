use super::*;
use std::io::{self, BufRead};

fn reference() -> PluginSession {
    // Same native-reference prerequisite as Core's reference_pin tests.
    let executable = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fabric-local-target");
    let manifest = serde_json::from_value(json!({
        "id":"local","version":"0.1.0","kind":"target","protocol":{"min":1,"max":1},
        "executable":executable,"args":[],"capabilities":["target.execute"]
    }))
    .unwrap();
    PluginSession::open(
        &manifest,
        Duration::from_secs(5),
        &CancellationToken::default(),
    )
    .unwrap()
}

fn evidence(error: &FabricError) -> Value {
    assert_eq!(error.kind, ErrorKind::Output);
    assert!(error.message.len() < 2048, "{error}");
    serde_json::from_str(error.message.split_once(" Timeout evidence: ").unwrap().1).unwrap()
}

fn wait_until(mut observed: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !observed() {
        assert!(
            Instant::now() < deadline,
            "owned child did not reach the required state"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn reaped(pid: u32) {
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn reference_receipt_timeout_observes_owned_child_before_teardown() {
    let mut session = reference();
    let plan = session
        .call(
            "target.inspect",
            json!({
                "placement":"local","run_id":"diagnostic","grant":"g1",
                "actions":[{"primitive":"process.inspect"}],"context":{}
            }),
        )
        .unwrap();
    assert_eq!(
        plan,
        json!({"grant":"g1","action":{"primitive":"process.inspect"}})
    );
    session.bound_target_operations();
    let pid = session.group.child.id();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
    // Confirm the actual direct child stopped, not an inferred serving PID.
    // The session's existing group Drop remains the bounded panic cleanup owner.
    wait_until(|| {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(result, 0);
        #[cfg(target_os = "macos")]
        let observed_pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let observed_pid = unsafe { info.si_pid() };
        observed_pid == pid as i32
    });
    let request_id = session.next_id;
    let error = session
        .call(
            "target.completed",
            json!({
                "grant":"g1","evidence":{"private":"ISSUE76_PRIVATE_EVIDENCE"}
            }),
        )
        .unwrap_err();
    assert!(error.message.starts_with(&format!(
        "Plugin: timeout. RPC target.completed (request {request_id}, phase response_read,"
    )));
    assert!(!error.message.contains("ISSUE76_PRIVATE_EVIDENCE"));
    let snapshot = evidence(&error);
    assert_eq!(snapshot["request_id"], request_id);
    assert_eq!(snapshot["method"], "target.completed");
    assert_eq!(snapshot["phase"], "response_read");
    assert!(snapshot["sent_bytes"].as_u64().unwrap() > 0);
    assert_eq!(snapshot["sent_bytes"], snapshot["total_bytes"]);
    assert_eq!(snapshot["received_bytes"], 0);
    assert_eq!(snapshot["buffered_bytes"], 0);
    assert_eq!(snapshot["configured_budget_us"], 5_000_000);
    assert_eq!(snapshot["effective_budget_us"], 1_000_000);
    assert_eq!(snapshot["remaining_budget_at_call_us"], 1_000_000);
    assert_eq!(snapshot["deadline_scope"], "per_request");
    assert_eq!(snapshot["negotiated_budget"], "not_in_protocol");
    assert!(snapshot["call_elapsed_us"].as_u64().unwrap() >= 1_000_000);
    assert_eq!(snapshot["child"]["pid"], pid);
    assert_eq!(snapshot["child"]["spawn_parent_pid"], std::process::id());
    assert_eq!(snapshot["child"]["pgid"], pid);
    assert_eq!(snapshot["child"]["relationship"], "owned_direct_child");
    #[cfg(target_os = "macos")]
    assert_eq!(
        snapshot["child"]["waitid_wnowait"],
        json!({"state":"stopped","signal":libc::SIGSTOP})
    );
    #[cfg(not(target_os = "macos"))]
    assert_eq!(
        snapshot["child"]["waitid_wnowait"],
        json!({"state":"not_exited"})
    );
    assert_eq!(
        snapshot["child"]["try_wait"],
        json!({
            "state":"UNKNOWN","reason":"not_called_preserve_group_pid_anchor"
        })
    );
    assert!(!session.group.exited().unwrap());
    drop(session);
    reaped(pid);
}

#[test]
fn reference_buffered_reply_count_does_not_consume_or_disclose_bytes() {
    let mut session = reference();
    let pid = session.group.child.id();
    let request = json!({
        "jsonrpc":"2.0","id":session.next_id,"method":"target.inspect","params":{
            "placement":"local","run_id":"diagnostic","grant":"ISSUE76_BUFFER_CANARY",
            "actions":[{"primitive":"process.inspect"}],"context":{}
        }
    });
    writeln!(session.input.as_mut().unwrap(), "{request}").unwrap();
    wait_until(|| match session.output.fill_buf() {
        Ok(bytes) => !bytes.is_empty(),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            false
        }
        Err(error) => panic!("reference response unavailable: {error}"),
    });
    let buffered = session.output.buffer().len();
    assert!(buffered > 0);
    // Inject only an expired deadline; bytes come from the actual reference child.
    session.timeout = Duration::ZERO;
    let error = session.call("health", json!({})).unwrap_err();
    let snapshot = evidence(&error);
    assert_eq!(snapshot["phase"], "request_write");
    assert_eq!(snapshot["sent_bytes"], 0);
    assert_eq!(snapshot["received_bytes"], 0);
    assert_eq!(snapshot["buffered_bytes"], buffered);
    assert_eq!(session.output.buffer().len(), buffered);
    assert!(!error.message.contains("ISSUE76_BUFFER_CANARY"));
    drop(session);
    reaped(pid);
}

#[test]
fn reference_exit_snapshot_keeps_the_process_group_pid_anchor() {
    let mut session = reference();
    let pid = session.group.child.id();
    assert_eq!(
        session.call("shutdown", json!({})).unwrap(),
        json!({"ready":true})
    );
    session.input.take();
    wait_until(|| session.group.exited().unwrap());
    let snapshot = session.group.timeout_snapshot();
    assert_eq!(snapshot["pid"], pid);
    assert_eq!(
        snapshot["waitid_wnowait"],
        json!({"state":"exited","code":0})
    );
    assert_eq!(snapshot["try_wait"]["state"], "UNKNOWN");
    assert!(session.group.exited().unwrap());
    drop(session);
    reaped(pid);
}

#[test]
fn reference_lost_anchor_reports_numeric_error_without_group_lookup() {
    let mut session = reference();
    assert_eq!(
        session.call("shutdown", json!({})).unwrap(),
        json!({"ready":true})
    );
    session.input.take();
    assert!(session.group.child.wait().unwrap().success());
    let snapshot = session.group.timeout_snapshot();
    assert_eq!(
        snapshot["waitid_wnowait"],
        json!({
            "state":"UNKNOWN","reason":"waitid_failed","errno":libc::ECHILD
        })
    );
    assert_eq!(snapshot["pgid"], Value::Null);
    assert_eq!(snapshot["pgid_state"], "UNKNOWN_anchor_lost");
}
