//! Versioned incoming connector data. No field carries execution authority.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedTrigger {
    pub version: u32,
    pub source_id: String,
    pub event_id: String,
    pub tenant_id: String,
    pub idempotency_key: String,
    pub scope: String,
    pub route: String,
    pub data: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncomingEvent {
    pub body: String,
    pub timestamp: i64,
    pub expires_at: i64,
    pub nonce: String,
    pub authorization: String,
}

/// Operator-selected policy; secret_ref is an absolute owner-only file, never a secret value.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorPolicy {
    pub source_id: String,
    pub tenant_id: String,
    pub scope: String,
    pub route: String,
    pub auth: String,
    pub secret_ref: String,
    pub max_payload_bytes: usize,
    pub max_age_seconds: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorInput {
    pub version: u32,
    pub request: IncomingEvent,
    pub policy: ConnectorPolicy,
    pub now: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorCapabilities {
    pub version: u32,
    pub optional: Vec<String>,
}

/// Only authenticated delivery metadata reaches durable admission; no authorization/body bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorDelivery {
    pub namespace: String,
    pub nonce: String,
    pub expires_at: i64,
    pub rate_limit: u32,
}

pub fn connector_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}
