use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{ErrorKind, FabricError};

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$ (
        /// An opaque, caller-supplied ID: 1–128 ASCII letters, digits, `_` or `-`.
        /// Validation makes this a single path component, not proof of path ownership.
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            fn validate(value: &str) -> Result<(), FabricError> {
                if value.is_empty() || value.len() > 128
                    || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                {
                    return Err(FabricError {
                        kind: ErrorKind::InvalidId,
                        message: concat!(stringify!($name), " must contain 1–128 ASCII letters, digits, '_' or '-'").into(),
                    });
                }
                Ok(())
            }
        }

        impl TryFrom<String> for $name {
            type Error = FabricError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::validate(&value)?;
                Ok(Self(value))
            }
        }

        impl FromStr for $name {
            type Err = FabricError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::validate(value)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    )+};
}

ids!(
    ProjectId,
    PreparedStateId,
    RunId,
    BatchId,
    WorkspaceId,
    ArtifactId,
    ArtifactManifestId
);
