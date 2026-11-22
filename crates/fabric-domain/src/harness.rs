use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{ErrorKind, FabricError};

macro_rules! harness_evidence {
    ($name:ident, $validate:expr, $kind:ident, $message:literal) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn validate(value: &str) -> Result<(), FabricError> {
                if !($validate)(value) {
                    return Err(FabricError {
                        kind: ErrorKind::$kind,
                        message: $message.into(),
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
    };
}

// Syntax is extensible; adapter registration, not this type, decides availability.
harness_evidence!(
    HarnessId,
    |value: &str| !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
    InvalidId,
    "HarnessId must contain 1–128 lowercase ASCII letters, digits or '-'"
);

// Preserve exact probe evidence rather than interpreting vendor version formats.
harness_evidence!(
    HarnessVersion,
    |value: &str| !value.trim().is_empty()
        && value.len() <= 1024
        && !value.chars().any(char::is_control),
    UnsupportedVersion,
    "HarnessVersion must contain 1–1024 bytes of nonempty version evidence without control characters"
);
