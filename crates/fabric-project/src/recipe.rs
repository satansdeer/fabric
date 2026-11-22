//! Validated, non-executing recipes and canonical preparation identities.

use std::collections::BTreeMap;
use std::io::{self, Write};

use fabric_domain::{ErrorKind, FabricError};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bump when the preparation layout or canonical fingerprint encoding changes.
pub const PREPARATION_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct Recipe(RecipeData);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipeData {
    version: u32,
    steps: Vec<Step>,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    environment: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    argv: Vec<String>,
}

impl Step {
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    #[serde(default = "default_max_duration_seconds")]
    max_duration_seconds: u64,
}

fn default_max_duration_seconds() -> u64 {
    1200
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_duration_seconds: default_max_duration_seconds(),
        }
    }
}

/// Safe to persist/display: environment values are consumed only by the hasher.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct PreparationFingerprint {
    pub format_version: u32,
    pub source_commit_sha: String,
    pub recipe_digest: String,
    pub environment_digest: String,
    pub preparation_key: String,
}

impl Recipe {
    /// Parse without executing commands, reading ambient environment or doing I/O.
    pub fn parse(input: &str) -> Result<Self, FabricError> {
        let mut recipe: RecipeData = toml::from_str(input).map_err(|_| {
            // TOML's diagnostic includes source excerpts, which may contain secrets.
            invalid_recipe("Invalid recipe TOML or shape; use version = 1, argv steps, optional limits and environment names only.")
        })?;
        recipe.validate()?;
        recipe.environment.sort_unstable();
        if recipe.environment.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid_recipe("Recipe environment names must be unique."));
        }
        Ok(Self(recipe))
    }

    pub fn steps(&self) -> &[Step] {
        &self.0.steps
    }

    pub fn max_duration_seconds(&self) -> u64 {
        self.0.limits.max_duration_seconds
    }

    pub fn environment(&self) -> &[String] {
        &self.0.environment
    }
}

impl RecipeData {
    fn validate(&self) -> Result<(), FabricError> {
        if self.version != 1 {
            return Err(invalid_recipe("Only recipe version 1 is supported."));
        }
        if self.steps.is_empty() {
            return Err(invalid_recipe(
                "Recipe must contain at least one argv step.",
            ));
        }
        for step in &self.steps {
            if step.argv.first().is_none_or(|arg| arg.trim().is_empty()) {
                return Err(invalid_recipe(
                    "Each step needs a nonempty argv executable.",
                ));
            }
            if step.argv.iter().any(|arg| arg.contains('\0')) {
                return Err(invalid_recipe("Recipe argv must not contain NUL bytes."));
            }
        }
        if self.limits.max_duration_seconds == 0 {
            return Err(invalid_recipe("max_duration_seconds must be positive."));
        }
        for name in &self.environment {
            let mut bytes = name.bytes();
            if !bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(invalid_recipe(
                    "Environment names must match [A-Za-z_][A-Za-z0-9_]*.",
                ));
            }
        }
        Ok(())
    }
}

impl Recipe {
    /// Only declared names are selected from the caller's environment snapshot.
    /// Unset (null) differs from empty. No raw values leave this call in metadata.
    pub fn fingerprint(
        &self,
        commit_sha: &str,
        environment: &BTreeMap<String, String>,
    ) -> Result<PreparationFingerprint, FabricError> {
        if ![40, 64].contains(&commit_sha.len())
            || !commit_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid_recipe(
                "Preparation requires a full 40- or 64-digit commit SHA, not a ref.",
            ));
        }
        // Serialize a sorted name/value map directly into SHA-256; do not buffer secrets.
        let selected = SelectedEnvironment {
            names: &self.0.environment,
            values: environment,
        };
        let source_commit_sha = commit_sha.to_ascii_lowercase();
        let recipe_digest = digest("fabric.recipe.v1", self)?;
        let environment_digest = digest("fabric.environment.v1", &selected)?;
        let preparation_key = digest(
            "fabric.preparation.v1",
            &(
                PREPARATION_FORMAT_VERSION,
                &source_commit_sha,
                &recipe_digest,
                &environment_digest,
            ),
        )?;
        Ok(PreparationFingerprint {
            format_version: PREPARATION_FORMAT_VERSION,
            source_commit_sha,
            recipe_digest,
            environment_digest,
            preparation_key,
        })
    }
}

struct SelectedEnvironment<'a> {
    names: &'a [String],
    values: &'a BTreeMap<String, String>,
}

impl Serialize for SelectedEnvironment<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.names.len()))?;
        for name in self.names {
            map.serialize_entry(name, &self.values.get(name))?;
        }
        map.end()
    }
}
struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn digest(domain: &str, value: &impl Serialize) -> Result<String, FabricError> {
    let mut writer = HashWriter(Sha256::new());
    writer.0.update(domain.as_bytes());
    writer.0.update([0]);
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| invalid_recipe("Cannot encode canonical preparation identity."))?;
    Ok(format!("{:x}", writer.0.finalize()))
}

fn invalid_recipe(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidRecipe,
        message: message.into(),
    }
}
