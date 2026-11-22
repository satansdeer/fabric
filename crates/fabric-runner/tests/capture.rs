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
