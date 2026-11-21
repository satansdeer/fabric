//! I/O-free error contract shared by Fabric's packages.

use std::fmt;

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidArguments,
    Output,
}

#[derive(Debug, Serialize)]
pub struct FabricError {
    pub kind: ErrorKind,
    pub message: String,
}

impl fmt::Display for FabricError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FabricError {}
