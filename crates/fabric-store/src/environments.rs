use super::*;
use fabric_domain::{
    EnvironmentCapabilities, EnvironmentDesired, EnvironmentId, EnvironmentRuntime,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    pub environment_id: EnvironmentId,
    pub owner: String,
    pub owner_uid: u32,
    pub target_json: String,
    pub capabilities: EnvironmentCapabilities,
    /// Core-captured descriptor identities, not target-provided paths or receipts.
    pub resource_json: Option<String>,
    pub desired: EnvironmentDesired,
    pub runtime: EnvironmentRuntime,
    pub observed_at: i64,
    pub observation: String,
    pub destroy_intent: bool,
}

impl Store {
    pub fn environment(&self, id: &EnvironmentId) -> Result<Option<EnvironmentRecord>> {
        let raw = self.connection.query_row(
            "SELECT owner,owner_uid,target_json,capabilities_json,resource_json,desired,runtime,observed_at,observation,destroy_intent FROM environments WHERE environment_id=?1",
            [id.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?, row.get::<_, bool>(9)?)),
        ).optional()?;
        raw.map(
            |(
                owner,
                owner_uid,
                target_json,
                caps,
                resource_json,
                desired,
                runtime,
                observed_at,
                observation,
                destroy_intent,
            )| {
                Ok(EnvironmentRecord {
                    environment_id: id.clone(),
                    owner,
                    owner_uid,
                    target_json,
                    capabilities: serde_json::from_str(&caps)
                        .map_err(|_| StoreError::InvalidRecord("environment capabilities"))?,
                    resource_json,
                    desired: match desired.as_str() {
                        "available" => EnvironmentDesired::Available,
                        "destroyed" => EnvironmentDesired::Destroyed,
                        _ => return Err(StoreError::InvalidRecord("environment desired state")),
                    },
                    runtime: match runtime.as_str() {
                        "available" => EnvironmentRuntime::Available,
                        "absent" => EnvironmentRuntime::Absent,
                        "unknown" => EnvironmentRuntime::Unknown,
                        _ => return Err(StoreError::InvalidRecord("environment runtime state")),
                    },
                    observed_at,
                    observation,
                    destroy_intent,
                })
            },
        )
        .transpose()
    }

    pub fn create_environment(&self, record: &EnvironmentRecord) -> Result<()> {
        if record.owner.is_empty()
            || record.owner.len() > 128
            || !record
                .owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || record.target_json.len() > 16384
            || record.observed_at < 0
            || record.resource_json.is_some()
            || record.destroy_intent
            || record.desired != EnvironmentDesired::Available
            || record.runtime != EnvironmentRuntime::Unknown
        {
            return Err(StoreError::InvalidRecord(
                "invalid initial environment ownership",
            ));
        }
        let caps = serde_json::to_string(&record.capabilities)
            .map_err(|_| StoreError::InvalidRecord("environment capabilities"))?;
        self.connection.execute(
            "INSERT INTO environments(environment_id,owner,owner_uid,target_json,capabilities_json,desired,runtime,observed_at,observation) VALUES(?1,?2,?3,?4,?5,'available','unknown',?6,?7)",
            params![record.environment_id.as_str(),record.owner,record.owner_uid,record.target_json,caps,record.observed_at,record.observation],
        )?;
        Ok(())
    }

    pub fn own_environment_resource(
        &self,
        id: &EnvironmentId,
        resource: &str,
        now: i64,
    ) -> Result<()> {
        if resource.len() > 8192 {
            return Err(StoreError::InvalidRecord("environment resource too large"));
        }
        let count = self.connection.execute(
            "UPDATE environments SET resource_json=?2,runtime='available',observed_at=?3,observation='owned_home_verified' WHERE environment_id=?1 AND resource_json IS NULL AND desired='available' AND destroy_intent=0",
            params![id.as_str(),resource,now],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub fn observe_environment(
        &self,
        id: &EnvironmentId,
        runtime: EnvironmentRuntime,
        observation: &str,
        now: i64,
    ) -> Result<()> {
        let runtime = match runtime {
            EnvironmentRuntime::Available => "available",
            EnvironmentRuntime::Absent => "absent",
            EnvironmentRuntime::Unknown => "unknown",
        };
        if observation.len() > 1024 || now < 0 {
            return Err(StoreError::InvalidRecord("invalid environment observation"));
        }
        if self.connection.execute("UPDATE environments SET runtime=?2,observation=?3,observed_at=?4 WHERE environment_id=?1", params![id.as_str(),runtime,observation,now])? != 1 { return Err(StoreError::NotFound); }
        Ok(())
    }

    pub fn intent_destroy_environment(
        &self,
        id: &EnvironmentId,
        resource: &str,
        now: i64,
    ) -> Result<()> {
        if self.connection.execute(
            "UPDATE environments SET desired='destroyed',destroy_intent=1,runtime='unknown',observation='destroy_intent_committed',observed_at=?3 WHERE environment_id=?1 AND resource_json=?2",
            params![id.as_str(),resource,now],
        )? != 1 { return Err(StoreError::Conflict); }
        Ok(())
    }

    pub fn bind_run_environment(&self, run: &RunId, id: &EnvironmentId) -> Result<()> {
        self.connection.execute(
            "INSERT INTO run_environments(run_id,environment_id) VALUES(?1,?2)",
            params![run.as_str(), id.as_str()],
        )?;
        Ok(())
    }

    pub fn run_environment(&self, run: &RunId) -> Result<Option<EnvironmentId>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 18 {
            return Ok(None);
        }
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT environment_id FROM run_environments WHERE run_id=?1",
                [run.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| id.parse().map_err(StoreError::Domain))
            .transpose()
    }

    pub fn environment_runs(&self, id: &EnvironmentId) -> Result<Vec<RunId>> {
        let mut statement = self.connection.prepare(
            "SELECT run_id FROM run_environments WHERE environment_id=?1 ORDER BY run_id",
        )?;
        let rows = statement.query_map([id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|id| id?.parse().map_err(StoreError::Domain))
            .collect()
    }
}
