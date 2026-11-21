# Progress

## FAB-01 — Rust workspace and doctor command

- Behavioral claim: `fabric doctor --json` deterministically reports the host OS/architecture, explicit macOS support, and executable Git/OMP discovery without executing tools, accessing the network, or modifying user data.
- RED: compiled `crates/fabric-cli/tests/doctor.rs` with `rustc --edition=2024 --test` and `CARGO_BIN_EXE_fabric` pointing to the not-yet-created binary; the test failed with `NotFound` (1 failed, exit 101) before production files existed.
- GREEN: `cargo test --offline -p fabric-cli --test doctor` passed all 5 tests. They cover exact JSON with no PATH, structured argument errors, help/version/human output, stable read-only executable discovery, and symlink/relative PATH handling.
- Full checks: `cargo clippy --offline --workspace --all-targets -- -D warnings` passed; `cargo test --offline --workspace` passed (5 integration tests; all package/doc-test targets passed). Initial `cargo fmt --check` found test formatting; formatting was corrected with targeted edits and the repeated `cargo fmt --check` passed.
- CLI smoke: parsed actual `./target/debug/fabric doctor --json` output with Python and asserted `os=macos`, `supported=true`, `primary_supported_os=macos`, and Git/OMP ordering. Both local tools were discovered; neither was executed.
- Scope: seven specified workspace packages, three direct external dependencies (clap, serde, serde_json), lockfile, structured domain errors, doctor implementation, integration tests and README. Other package boundaries intentionally contain no later-story behavior. No supplied requirement documents or PRD status changed; no commit made.
- Risks: executable discovery is not a launch/version compatibility probe. Non-macOS platforms are reported unsupported and were not verified. No paid/real OMP call occurred. The outer verifier owns clean-checkout verification and completion status.
- Status: READY_FOR_VERIFICATION.
