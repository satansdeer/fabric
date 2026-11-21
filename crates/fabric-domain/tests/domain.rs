use fabric_domain::{
    ArtifactId, ArtifactManifestId, BatchId, ErrorKind, PreparedStateId, PreparedStateStatus,
    ProjectId, RetentionMode, RetentionPolicy, RunId, RunState, StoragePolicy, WorkspaceId,
    WorkspaceState,
};

#[test]
fn typed_ids_validate_safe_components_and_round_trip() {
    macro_rules! check_id {
        ($id:ty) => {{
            let id: $id = "run_0123-AB".parse().unwrap();
            assert_eq!(id.as_str(), "run_0123-AB");
            assert_eq!(id.to_string(), "run_0123-AB");
            assert_eq!(serde_json::to_string(&id).unwrap(), "\"run_0123-AB\"");
            assert_eq!(serde_json::from_str::<$id>("\"run_0123-AB\"").unwrap(), id);
            for invalid in ["", ".", "..", "../run", "/run", "a\\b", "a b", "a\n", "é"] {
                assert_eq!(
                    invalid.parse::<$id>().unwrap_err().kind,
                    ErrorKind::InvalidId
                );
                assert!(
                    serde_json::from_str::<$id>(&serde_json::to_string(invalid).unwrap()).is_err()
                );
            }
            assert!("a".repeat(128).parse::<$id>().is_ok());
            assert_eq!(
                "a".repeat(129).parse::<$id>().unwrap_err().kind,
                ErrorKind::InvalidId
            );
        }};
    }
    check_id!(ProjectId);
    check_id!(PreparedStateId);
    check_id!(RunId);
    check_id!(BatchId);
    check_id!(WorkspaceId);
    check_id!(ArtifactId);
    check_id!(ArtifactManifestId);
}

#[test]
fn run_transitions_follow_the_spec_and_reject_terminal_rewrites() {
    use RunState::*;
    let states = [
        Queued,
        Preparing,
        Running,
        Finalizing,
        Succeeded,
        Failed,
        Cancelled,
        NeedsAttention,
    ];
    for from in states {
        for to in states {
            let allowed = from == to
                || matches!(
                    (from, to),
                    (Queued, Preparing)
                        | (Preparing, Running)
                        | (Running, Finalizing)
                        | (Finalizing, Succeeded | Failed | Cancelled | NeedsAttention)
                );
            let mut state = from;
            match state.transition(to) {
                Ok(()) => {
                    assert!(allowed, "{from:?} -> {to:?}");
                    assert_eq!(state, to);
                }
                Err(error) => {
                    assert!(!allowed, "{from:?} -> {to:?}");
                    assert_eq!(error.kind, ErrorKind::InvalidTransition);
                    assert_eq!(state, from);
                }
            }
        }
    }
    for state in states {
        assert_eq!(
            state.is_terminal(),
            matches!(state, Succeeded | Failed | Cancelled | NeedsAttention)
        );
    }
}

#[test]
fn ready_prepared_states_are_immutable_and_failed_states_are_not_reused() {
    use PreparedStateStatus::*;
    for from in [Preparing, Ready, Failed] {
        for to in [Preparing, Ready, Failed] {
            let mut state = from;
            let allowed = from == to || matches!((from, to), (Preparing, Ready | Failed));
            assert_eq!(state.transition(to).is_ok(), allowed);
            assert_eq!(state, if allowed { to } else { from });
        }
    }
    let mut ready = Ready;
    assert_eq!(
        ready.transition(Failed).unwrap_err().kind,
        ErrorKind::InvalidTransition
    );
}

#[test]
fn workspace_transitions_preserve_recovery_and_reclaimed_is_terminal() {
    use WorkspaceState::*;
    let states = [
        Active,
        CleanupPending,
        Reclaimed,
        Retained,
        RecoveryRequired,
    ];
    for from in states {
        for to in states {
            let allowed = from == to
                || matches!(
                    (from, to),
                    (Active, CleanupPending | Retained | RecoveryRequired)
                        | (CleanupPending, Reclaimed | Retained | RecoveryRequired)
                        | (Retained, CleanupPending | RecoveryRequired)
                        | (RecoveryRequired, CleanupPending | Retained)
                );
            let mut state = from;
            match state.transition(to) {
                Ok(()) => {
                    assert!(allowed, "{from:?} -> {to:?}");
                    assert_eq!(state, to);
                }
                Err(error) => {
                    assert!(!allowed, "{from:?} -> {to:?}");
                    assert_eq!(error.kind, ErrorKind::InvalidTransition);
                    assert_eq!(state, from);
                }
            }
        }
    }
    assert!(Active.transition(Reclaimed).is_err());
    assert!(RecoveryRequired.transition(Reclaimed).is_err());
}

#[test]
fn state_serialization_uses_spec_names_and_rejects_unknown_states() {
    assert_eq!(
        serde_json::to_string(&RunState::NeedsAttention).unwrap(),
        "\"NEEDS_ATTENTION\""
    );
    assert_eq!(
        serde_json::to_string(&WorkspaceState::RecoveryRequired).unwrap(),
        "\"RECOVERY_REQUIRED\""
    );
    assert_eq!(
        serde_json::to_string(&PreparedStateStatus::Ready).unwrap(),
        "\"READY\""
    );
    assert_eq!(
        serde_json::from_str::<RunState>("\"RUNNING\"").unwrap(),
        RunState::Running
    );
    assert!(serde_json::from_str::<RunState>("\"success\"").is_err());
    assert!(serde_json::from_str::<WorkspaceState>("\"DELETED\"").is_err());
    assert!(serde_json::from_str::<PreparedStateStatus>("\"ACTIVE\"").is_err());
}

#[test]
fn policy_defaults_match_the_spec_and_validate_without_io() {
    let policy = StoragePolicy::default();
    assert_eq!(policy.max_managed_bytes, 20 * 1024_u64.pow(3));
    assert_eq!(policy.min_free_bytes, 5 * 1024_u64.pow(3));
    assert_eq!(policy.max_workspace_bytes, 2 * 1024_u64.pow(3));
    assert_eq!(policy.max_log_bytes_per_run, 10 * 1024_u64.pow(2));
    assert_eq!(policy.max_artifact_bytes_per_run, 100 * 1024_u64.pow(2));
    assert_eq!(policy.retention, RetentionPolicy::default());
    assert_eq!(
        policy.retention.on_success,
        RetentionMode::CheckpointThenDiscard
    );
    assert_eq!(
        policy.retention.on_failure,
        RetentionMode::CheckpointThenRetainIfUnsafe
    );
    assert_eq!(policy.retention.recovery_retention_hours, 24);
    policy.validate().unwrap();
    assert_eq!(serde_json::from_str::<StoragePolicy>("{}").unwrap(), policy);
    let encoded = serde_json::to_string(&policy).unwrap();
    assert_eq!(
        serde_json::from_str::<StoragePolicy>(&encoded).unwrap(),
        policy
    );
}

#[test]
fn policy_validation_rejects_zero_inconsistent_and_overflowing_limits() {
    macro_rules! invalid {
        ($field:ident, $value:expr) => {{
            let policy = StoragePolicy {
                $field: $value,
                ..StoragePolicy::default()
            };
            assert_eq!(
                policy.validate().unwrap_err().kind,
                ErrorKind::InvalidPolicy
            );
        }};
    }
    invalid!(max_managed_bytes, 0);
    invalid!(min_free_bytes, 0);
    invalid!(max_workspace_bytes, 0);
    invalid!(max_log_bytes_per_run, 0);
    invalid!(max_artifact_bytes_per_run, 0);
    invalid!(max_workspace_bytes, u64::MAX);
    invalid!(max_log_bytes_per_run, u64::MAX);
    invalid!(max_artifact_bytes_per_run, u64::MAX);
    let overflow = StoragePolicy {
        max_managed_bytes: u64::MAX,
        max_workspace_bytes: u64::MAX,
        ..StoragePolicy::default()
    };
    assert_eq!(
        overflow.validate().unwrap_err().kind,
        ErrorKind::InvalidPolicy
    );
    for hours in [0, u64::MAX] {
        let retention = RetentionPolicy {
            recovery_retention_hours: hours,
            ..RetentionPolicy::default()
        };
        assert_eq!(
            retention.validate().unwrap_err().kind,
            ErrorKind::InvalidPolicy
        );
    }
    // The free-space reserve is outside Fabric's managed budget, not part of it.
    StoragePolicy {
        min_free_bytes: u64::MAX,
        ..StoragePolicy::default()
    }
    .validate()
    .unwrap();
}

#[test]
fn untrusted_policy_deserialization_rejects_bad_limits_and_unknown_fields() {
    for input in [
        r#"{"max_workspace_bytes":0}"#,
        r#"{"max_workspace_bytes":18446744073709551615}"#,
        r#"{"max_managed_bytes":-1}"#,
        r#"{"max_managed_bytes":1.5}"#,
        r#"{"max_log_bytes_per_run":18446744073709551616}"#,
        r#"{"max_logs_bytes_per_run":10}"#,
        r#"{"retention":{"recovery_retention_hours":0}}"#,
        r#"{"retention":{"on_failure":"discard"}}"#,
        r#"{"retention":{"recovery_retention_hour":24}}"#,
    ] {
        assert!(
            serde_json::from_str::<StoragePolicy>(input).is_err(),
            "{input}"
        );
    }
    assert!(serde_json::from_str::<RetentionPolicy>(r#"{"recovery_retention_hours":0}"#).is_err());
    assert!(
        serde_json::from_str::<RetentionPolicy>(
            r#"{"recovery_retention_hours":18446744073709551615}"#
        )
        .is_err()
    );
}
