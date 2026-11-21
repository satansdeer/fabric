use serde::{Deserialize, Deserializer, Serialize};

use crate::{ErrorKind, FabricError};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    CheckpointThenDiscard,
    CheckpointThenRetainIfUnsafe,
}

/// Both modes require durable, verified recovery capture before any deletion.
/// Retention expiry never overrides an active lease or missing recovery artifacts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RetentionPolicy {
    pub on_success: RetentionMode,
    pub on_failure: RetentionMode,
    pub recovery_retention_hours: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            on_success: RetentionMode::CheckpointThenDiscard,
            on_failure: RetentionMode::CheckpointThenRetainIfUnsafe,
            recovery_retention_hours: 24,
        }
    }
}

impl RetentionPolicy {
    pub fn validate(&self) -> Result<(), FabricError> {
        if self.recovery_retention_hours == 0
            || self.recovery_retention_hours.checked_mul(3600).is_none()
        {
            return Err(invalid_policy(
                "recovery_retention_hours must be positive and fit in seconds",
            ));
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for RetentionPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(remote = "RetentionPolicy", default, deny_unknown_fields)]
        struct Fields {
            on_success: RetentionMode,
            on_failure: RetentionMode,
            recovery_retention_hours: u64,
        }
        impl Default for Fields {
            fn default() -> Self {
                let policy = RetentionPolicy::default();
                Self {
                    on_success: policy.on_success,
                    on_failure: policy.on_failure,
                    recovery_retention_hours: policy.recovery_retention_hours,
                }
            }
        }
        let policy = Fields::deserialize(deserializer)?;
        policy.validate().map_err(serde::de::Error::custom)?;
        Ok(policy)
    }
}

/// Best-effort byte limits, not filesystem quotas or cleanup authorization.
/// Deserialization validates untrusted config. Call `validate` after direct edits.
/// Storage root ownership and filesystem checks belong to the I/O layers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoragePolicy {
    pub max_managed_bytes: u64,
    pub min_free_bytes: u64,
    pub max_workspace_bytes: u64,
    pub max_log_bytes_per_run: u64,
    pub max_artifact_bytes_per_run: u64,
    pub retention: RetentionPolicy,
}

impl Default for StoragePolicy {
    fn default() -> Self {
        Self {
            max_managed_bytes: 20 * 1024_u64.pow(3),
            min_free_bytes: 5 * 1024_u64.pow(3),
            max_workspace_bytes: 2 * 1024_u64.pow(3),
            max_log_bytes_per_run: 10 * 1024_u64.pow(2),
            max_artifact_bytes_per_run: 100 * 1024_u64.pow(2),
            retention: RetentionPolicy::default(),
        }
    }
}

impl StoragePolicy {
    pub fn validate(&self) -> Result<(), FabricError> {
        for (name, value) in [
            ("max_managed_bytes", self.max_managed_bytes),
            ("min_free_bytes", self.min_free_bytes),
            ("max_workspace_bytes", self.max_workspace_bytes),
            ("max_log_bytes_per_run", self.max_log_bytes_per_run),
            (
                "max_artifact_bytes_per_run",
                self.max_artifact_bytes_per_run,
            ),
        ] {
            if value == 0 {
                return Err(invalid_policy(format!("{name} must be positive")));
            }
        }
        let per_run = self
            .max_workspace_bytes
            .checked_add(self.max_log_bytes_per_run)
            .and_then(|bytes| bytes.checked_add(self.max_artifact_bytes_per_run));
        if per_run.is_none_or(|bytes| bytes > self.max_managed_bytes) {
            return Err(invalid_policy(
                "workspace, log and artifact limits combined must fit max_managed_bytes",
            ));
        }
        self.retention.validate()
    }
}

impl<'de> Deserialize<'de> for StoragePolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(remote = "StoragePolicy", default, deny_unknown_fields)]
        struct Fields {
            max_managed_bytes: u64,
            min_free_bytes: u64,
            max_workspace_bytes: u64,
            max_log_bytes_per_run: u64,
            max_artifact_bytes_per_run: u64,
            retention: RetentionPolicy,
        }
        impl Default for Fields {
            fn default() -> Self {
                let policy = StoragePolicy::default();
                Self {
                    max_managed_bytes: policy.max_managed_bytes,
                    min_free_bytes: policy.min_free_bytes,
                    max_workspace_bytes: policy.max_workspace_bytes,
                    max_log_bytes_per_run: policy.max_log_bytes_per_run,
                    max_artifact_bytes_per_run: policy.max_artifact_bytes_per_run,
                    retention: policy.retention,
                }
            }
        }
        let policy = Fields::deserialize(deserializer)?;
        policy.validate().map_err(serde::de::Error::custom)?;
        Ok(policy)
    }
}

fn invalid_policy(message: impl Into<String>) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidPolicy,
        message: message.into(),
    }
}
