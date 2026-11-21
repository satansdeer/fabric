# External verification

Operator platform override: macOS first. Verified on arm64 macOS; Rust/cargo 1.91.0. The parent orchestrator, not the implementer, owns this record and PRD status.

## FAB-01

- Reviewed doctor implementation, command/error dispatch and all five integration tests.
- Independently ran `cargo test --offline -p fabric-cli --test doctor`: 5 passed.
- Launched `target/debug/fabric doctor --json`: valid JSON, `os=macos`, `arch=aarch64`, `supported=true`, Git and OMP discovered.
- Launched `target/debug/fabric --help`: doctor/help/version exposed.
- Copied only Cargo manifests/lockfile and crates to a fresh temporary source tree; `cargo test --offline --workspace --manifest-path <temporary>/Cargo.toml` passed.
- Implementer supplied observed RED test evidence and passing fmt/clippy/workspace checks.

## Final demonstration constraint

`omp --version` reports `omp/19.1.0`; `omp --help` confirms the headless JSON interface. No paid/model invocation has been authorized or performed. The final real-OMP acceptance smoke remains a separate approval-gated requirement; do not claim the PoC demonstrated without it.

## FAB-02

- Reviewed typed-ID validation, pure state-transition graphs and policy validation/deserialization.
- Independently ran `cargo test --offline -p fabric-domain --test domain`: all 8 tests passed, including illegal transitions, immutable READY states and invalid/overflowing limits.
- Implementer supplied observed RED evidence and passing fmt/clippy/workspace checks (13 tests).
