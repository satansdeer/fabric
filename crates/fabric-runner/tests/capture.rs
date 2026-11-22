use std::fs::File;
use std::time::{Duration, Instant};

use fabric_runner::{CommandSpec, capture::Capture, execute};

fn command(script: &str) -> CommandSpec {
    CommandSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        environment: Vec::new(),
    }
}

#[test]
fn descendants_cannot_keep_capture_pipes_open_after_leader_exit() {
    let capture = Capture::new(
        tempfile::tempfile().unwrap(),
        tempfile::tempfile().unwrap(),
        Some(tempfile::tempfile().unwrap()),
        4096,
    )
    .unwrap();
    let start = Instant::now();
    let result = execute(
        &command("/bin/sleep 20 & printf '{}\\n'"),
        std::path::Path::new("/"),
        capture,
        &fabric_runner::ExecutionLimits::default(),
        &fabric_runner::CancellationToken::default(),
        None,
        |_| Ok(()),
    )
    .unwrap();
    assert!(result.status.success());
    assert_eq!(result.capture.unwrap().event_counts.parsed, 1);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn persistence_failure_still_drains_concurrent_streams_and_preserves_observed_exit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("readonly");
    std::fs::write(&path, "").unwrap();
    let capture = Capture::new(
        File::open(path).unwrap(),
        tempfile::tempfile().unwrap(),
        Some(tempfile::tempfile().unwrap()),
        4096,
    )
    .unwrap();
    let result = execute(
        &command("/usr/bin/head -c 131072 /dev/zero >&2 & /usr/bin/head -c 131072 /dev/zero; wait"),
        root.path(),
        capture,
        &fabric_runner::ExecutionLimits::default(),
        &fabric_runner::CancellationToken::default(),
        None,
        |_| Ok(()),
    )
    .unwrap();
    assert!(result.status.success());
    assert!(result.capture.is_err());
}

#[test]
fn timeout_escalates_a_term_ignoring_leader_and_reaps_its_observed_status() {
    use std::os::unix::process::ExitStatusExt;
    let capture = Capture::new(
        tempfile::tempfile().unwrap(),
        tempfile::tempfile().unwrap(),
        Some(tempfile::tempfile().unwrap()),
        4096,
    )
    .unwrap();
    let start = Instant::now();
    let result = execute(
        &command("trap '' TERM; exec /bin/sleep 20"),
        std::path::Path::new("/"),
        capture,
        &fabric_runner::ExecutionLimits {
            timeout: Duration::from_millis(100),
            term_grace: Duration::from_millis(50),
        },
        &fabric_runner::CancellationToken::default(),
        None,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(result.stop_reason, Some(fabric_runner::StopReason::Timeout));
    assert_eq!(result.status.code(), None);
    assert_eq!(result.status.signal(), Some(9));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(result.capture.is_ok());
}

#[test]
fn cancelled_token_never_launches_and_a_failed_start_callback_cannot_hang_drains() {
    let root = tempfile::tempdir().unwrap();
    let cancellation = fabric_runner::CancellationToken::default();
    cancellation.cancel();
    cancellation.cancel();
    for cancelled in [true, false] {
        let capture = Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            Some(tempfile::tempfile().unwrap()),
            4096,
        )
        .unwrap();
        let fresh = fabric_runner::CancellationToken::default();
        let start = Instant::now();
        let result = execute(
            &command("printf started > marker; /bin/sleep 20 & wait"),
            root.path(),
            capture,
            &fabric_runner::ExecutionLimits::default(),
            if cancelled { &cancellation } else { &fresh },
            None,
            |_| {
                Err(fabric_domain::FabricError {
                    kind: fabric_domain::ErrorKind::Output,
                    message: "cannot commit start".into(),
                })
            },
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        if cancelled {
            assert!(!root.path().join("marker").exists());
        }
    }
}

#[test]
fn native_fatal_zero_exit_is_observed_even_after_capture_is_full() {
    let harness = fabric_runner::FakeHarness::new("/unused".into()).unwrap();
    for limit in [256, 100_000] {
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        let capture = Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            Some(File::create(&events).unwrap()),
            limit,
        )
        .unwrap()
        .with_harness(&harness);
        let script = if limit == 256 {
            "head -c 100000 /dev/zero; printf '\\n{\"type\":\"failure\",\"code\":\"fixture_fatal\",\"message\":\"native fatal\"}\\n'"
        } else {
            "printf '{\"type\":\"failure\",\"code\":\"fixture_fatal\",\"message\":\"native fatal\"}\\n'"
        };
        let result = execute(
            &command(script),
            root.path(),
            capture,
            &fabric_runner::ExecutionLimits::default(),
            &fabric_runner::CancellationToken::default(),
            None,
            |_| Ok(()),
        )
        .unwrap();
        assert!(
            result.status.success(),
            "native evidence must not rewrite OS status"
        );
        let metadata = result.capture.unwrap();
        assert_eq!(metadata.native_failure.as_deref(), Some("fixture_fatal"));
        assert_eq!(metadata.normalized_event_count, 3);
        assert_eq!(metadata.truncated, limit == 256);
        assert!(metadata.persisted_bytes <= limit);
        let records: Vec<serde_json::Value> = std::fs::read_to_string(events)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[0]["event"]["type"], "started");
        if metadata.truncated {
            assert!(
                records
                    .iter()
                    .all(|record| record["event"]["type"] != "completed")
            );
            assert!((records.len() as u64) < metadata.normalized_event_count);
        } else {
            let terminal = &records.last().unwrap()["event"];
            assert_eq!(terminal["type"], "completed");
            assert_eq!(terminal["exit_code"], 0);
            assert_eq!(terminal["success"], false);
        }
    }
}

#[test]
fn process_terminal_events_are_observed_not_guessed_from_native_claims() {
    let harness = fabric_runner::FakeHarness::new("/unused".into()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events");
    let capture = Capture::new(
        tempfile::tempfile().unwrap(),
        tempfile::tempfile().unwrap(),
        Some(File::create(&path).unwrap()),
        100_000,
    )
    .unwrap()
    .with_harness(&harness);
    let result = execute(
        &command("printf '{\"type\":\"finished\",\"exit_code\":0}\\n'; exit 7"),
        root.path(),
        capture,
        &fabric_runner::ExecutionLimits::default(),
        &fabric_runner::CancellationToken::default(),
        None,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(result.status.code(), Some(7));
    let metadata = result.capture.unwrap();
    assert_eq!(metadata.normalized_event_count, 2);
    let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let terminal = &records.last().unwrap()["event"];
    assert_eq!(terminal["exit_code"], 7);
    assert_eq!(terminal["success"], false);
}
