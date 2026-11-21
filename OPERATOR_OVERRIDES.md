# Operator requirements

These explicit operator instructions supersede conflicting platform requirements in FABRIC_POC_SPEC.md and PRD.json.

- Primary supported platform: macOS, not Linux (operator instruction, 2026-11-22).
- Doctor must report macOS support accurately. Implement and verify macOS process-group supervision, cancellation, timeouts, storage metering and safe reclamation.
- Prefer macOS APFS clonefile copy-on-write forks where available, with an explicitly size-admitted independent-copy fallback. Never hardlink writable files.
- macOS automated acceptance is the primary CI gate. Linux support is not a prerequisite for this PoC.
- Preserve all platform-independent acceptance criteria and safety invariants.
- The outer orchestrator owns PRD completion status and independent verification. Each fresh implementation subagent implements exactly one dependency-ready story, following RALPH_PROMPT.md with this platform override.
- Local commits are permitted through the eval commit namespace. No push, remote repository creation, merge, deployment, or paid real-model smoke run is authorized by this document.
