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
        |_| Ok(()),
    )
    .unwrap();
    assert!(result.status.success());
    assert!(result.capture.is_err());
}
