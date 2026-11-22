use super::*;

#[derive(Clone, Debug)]
pub struct AsyncBatch {
    pub batch_id: BatchId,
    pub prepared_state_id: PreparedStateId,
    pub request_digest: String,
    pub concurrency: usize,
    pub options_json: String,
    pub fake_binary: Option<String>,
}

pub struct NewAsyncTask {
    pub task: BatchTask,
    pub prompt: Vec<u8>,
    pub harness: String,
    pub plan_json: String,
    pub consent_stamp: Option<String>,
    pub source: Option<SourceProvenance>,
}

pub struct AsyncTask {
    pub run_id: RunId,
    pub prompt: Vec<u8>,
    pub harness: String,
    pub plan_json: String,
    pub consent_stamp: Option<String>,
}

pub struct AsyncTaskStatus {
    pub task_id: String,
    pub run_id: RunId,
    pub queue_state: RunState,
    pub state: RunState,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct AsyncEvent {
    pub cursor: u64,
    pub run_id: RunId,
    pub state: RunState,
    pub created_at: i64,
}

pub type AsyncLogIdentity = (ProjectId, BatchId, RunState, String, i64, i64);

impl Store {
    /// Content-free identity for observers; never loads prompts, plans or results.
    pub fn async_log_identity(&self, run: &RunId) -> Result<Option<AsyncLogIdentity>> {
        Ok(self
            .connection
            .query_row(
                "SELECT p.project_id,b.batch_id,
             CASE WHEN q.state IN ('SUCCEEDED','FAILED','CANCELLED','NEEDS_ATTENTION')
               THEN q.state ELSE COALESCE(r.state,q.state) END,q.harness,b.created_at,
             COALESCE((SELECT created_at FROM async_events e
                       WHERE e.batch_id=b.batch_id AND e.run_id=q.run_id
                       ORDER BY cursor DESC LIMIT 1),b.created_at)
             FROM async_tasks q JOIN batch_tasks t USING(run_id)
             JOIN async_batches b USING(batch_id) JOIN batches a USING(batch_id)
             JOIN prepared_states p ON p.state_id=a.prepared_state_id
             LEFT JOIN runs r ON r.run_id=q.run_id WHERE q.run_id=?1",
                [run.as_str()],
                |row| {
                    Ok((
                        read_id(row, 0)?,
                        read_id(row, 1)?,
                        read_state(row, 2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?)
    }

    pub fn async_batch(&self, id: &BatchId) -> Result<Option<AsyncBatch>> {
        Ok(self.connection.query_row(
            "SELECT a.batch_id,b.prepared_state_id,a.request_digest,a.concurrency,a.options_json,a.fake_binary
             FROM async_batches a JOIN batches b USING(batch_id) WHERE a.batch_id=?1",
            [id.as_str()], read_batch,
        ).optional()?)
    }

    pub fn async_idempotency(&self, key: &str) -> Result<Option<AsyncBatch>> {
        Ok(self.connection.query_row(
            "SELECT a.batch_id,b.prepared_state_id,a.request_digest,a.concurrency,a.options_json,a.fake_binary
             FROM async_batches a JOIN batches b USING(batch_id) WHERE a.idempotency_key=?1",
            [key], read_batch,
        ).optional()?)
    }

    /// One transaction is the acceptance boundary, including immutable plans and prompt bytes.
    pub fn accept_async_batch(
        &mut self,
        batch: &AsyncBatch,
        key: Option<&str>,
        tasks: &[NewAsyncTask],
        now: i64,
    ) -> Result<BatchId> {
        if tasks.is_empty()
            || tasks.len() > 128
            || batch.concurrency == 0
            || batch.concurrency > 128
        {
            return Err(StoreError::InvalidRecord(
                "async batch requires 1-128 tasks and concurrency",
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(key) = key {
            let prior: Option<(String, String)> = tx
                .query_row(
                    "SELECT batch_id,request_digest FROM async_batches WHERE idempotency_key=?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((id, digest)) = prior {
                return if digest == batch.request_digest {
                    Ok(BatchId::try_from(id)?)
                } else {
                    Err(StoreError::Conflict)
                };
            }
        }
        if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM prepared_states WHERE state_id=?1 AND status='READY')",
            [batch.prepared_state_id.as_str()],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(StoreError::InvalidRecord(
                "async batch requires a READY prepared state",
            ));
        }
        tx.execute(
            "INSERT INTO batches(batch_id,prepared_state_id) VALUES(?1,?2)",
            params![batch.batch_id.as_str(), batch.prepared_state_id.as_str()],
        )?;
        tx.execute(
            "INSERT INTO async_batches(batch_id,idempotency_key,request_digest,concurrency,options_json,fake_binary,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![batch.batch_id.as_str(),key,batch.request_digest,batch.concurrency as i64,batch.options_json,batch.fake_binary,now],
        )?;
        for (position, task) in tasks.iter().enumerate() {
            tx.execute(
                "INSERT INTO batch_tasks(batch_id,position,task_id,run_id,prompt_file) VALUES(?1,?2,?3,?4,'durable-input')",
                params![batch.batch_id.as_str(),position as i64,task.task.task_id,task.task.run_id.as_str()],
            )?;
            insert_task_source(
                &tx,
                &task.task.run_id,
                &batch.prepared_state_id,
                task.source.as_ref(),
            )?;
            tx.execute(
                "INSERT INTO async_tasks(run_id,prompt,harness,plan_json,consent_stamp,state) VALUES(?1,?2,?3,?4,?5,'QUEUED')",
                params![task.task.run_id.as_str(),task.prompt,task.harness,task.plan_json,task.consent_stamp],
            )?;
        }
        tx.commit()?;
        Ok(batch.batch_id.clone())
    }

    pub fn async_unfinished_batches(&self) -> Result<Vec<BatchId>> {
        let mut statement = self.connection.prepare(
            "SELECT a.batch_id FROM async_batches a JOIN batches b USING(batch_id)
             WHERE b.succeeded IS NULL ORDER BY a.created_at,a.batch_id",
        )?;
        Ok(statement
            .query_map([], |r| read_id(r, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn async_tasks(&self, batch: &BatchId) -> Result<Vec<AsyncTaskStatus>> {
        let mut statement = self.connection.prepare(
            "SELECT t.task_id,t.run_id,q.state,
             CASE WHEN q.state IN ('SUCCEEDED','FAILED','CANCELLED','NEEDS_ATTENTION') THEN q.state ELSE COALESCE(r.state,q.state) END,
             t.result,t.error FROM batch_tasks t JOIN async_tasks q USING(run_id)
             LEFT JOIN runs r USING(run_id) WHERE t.batch_id=?1 ORDER BY t.position",
        )?;
        Ok(statement
            .query_map([batch.as_str()], |r| {
                Ok(AsyncTaskStatus {
                    task_id: r.get(0)?,
                    run_id: read_id(r, 1)?,
                    queue_state: read_state(r, 2)?,
                    state: read_state(r, 3)?,
                    result: r.get(4)?,
                    error: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn async_task(&self, run: &RunId) -> Result<AsyncTask> {
        self.connection.query_row(
            "SELECT run_id,prompt,harness,plan_json,consent_stamp FROM async_tasks WHERE run_id=?1",
            [run.as_str()], |r| Ok(AsyncTask {
                run_id:read_id(r,0)?,prompt:r.get(1)?,harness:r.get(2)?,plan_json:r.get(3)?,consent_stamp:r.get(4)?,
            }),
        ).optional()?.ok_or(StoreError::NotFound)
    }

    pub fn task_prepared_state(&self, run: &RunId) -> Result<PreparedStateId> {
        Ok(self.connection.query_row(
            "SELECT prepared_state_id FROM batch_task_sources WHERE run_id=?1",
            [run.as_str()],
            |row| read_id(row, 0),
        )?)
    }

    pub fn task_source(&self, run: &RunId) -> Result<Option<SourceProvenance>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 11 {
            return Ok(None);
        }
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT provenance_json FROM batch_task_sources WHERE run_id=?1",
                [run.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        json.map(|json| {
            serde_json::from_str(&json)
                .map_err(|_| StoreError::InvalidRecord("invalid task source provenance"))
        })
        .transpose()
    }

    /// A durable claim precedes every side effect; an interrupted claim is never replayable.
    pub fn claim_async_task(&self, run: &RunId) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE async_tasks SET state='PREPARING' WHERE run_id=?1 AND state='QUEUED'",
            [run.as_str()],
        )? == 1)
    }

    pub fn complete_async_task(
        &mut self,
        run: &RunId,
        state: RunState,
        outcome: std::result::Result<&str, &str>,
        queued_only: bool,
    ) -> Result<bool> {
        if !terminal(state) {
            return Err(StoreError::InvalidRecord(
                "async completion requires terminal state",
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE async_tasks SET state=?1 WHERE run_id=?2
             AND state NOT IN ('SUCCEEDED','FAILED','CANCELLED','NEEDS_ATTENTION') AND (NOT ?3 OR state='QUEUED')",
            params![state.sql_name(),run.as_str(),queued_only],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        let (result, error) = match outcome {
            Ok(value) => (Some(value), None),
            Err(value) => (None, Some(value)),
        };
        if tx.execute(
            "UPDATE batch_tasks SET result=?1,error=?2 WHERE run_id=?3 AND result IS NULL AND error IS NULL",
            params![result,error,run.as_str()],
        )? != 1 { return Err(StoreError::Conflict); }
        tx.execute(
            "UPDATE batches SET succeeded=NOT EXISTS(
                SELECT 1 FROM batch_tasks t JOIN async_tasks q USING(run_id) WHERE t.batch_id=batches.batch_id AND q.state!='SUCCEEDED')
             WHERE batch_id=(SELECT batch_id FROM batch_tasks WHERE run_id=?1) AND succeeded IS NULL
             AND NOT EXISTS(SELECT 1 FROM batch_tasks t JOIN async_tasks q USING(run_id)
               WHERE t.batch_id=batches.batch_id AND q.state IN ('QUEUED','PREPARING','RUNNING','FINALIZING'))",
            [run.as_str()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn async_events(
        &self,
        batch: &BatchId,
        run: Option<&RunId>,
        after: u64,
    ) -> Result<Vec<AsyncEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT cursor,run_id,state,created_at FROM async_events
             WHERE batch_id=?1 AND (?2 IS NULL OR run_id=?2) AND cursor>?3 ORDER BY cursor LIMIT 256",
        )?;
        Ok(statement
            .query_map(
                params![
                    batch.as_str(),
                    run.map(RunId::as_str),
                    i64::try_from(after).unwrap_or(i64::MAX)
                ],
                |r| {
                    Ok(AsyncEvent {
                        cursor: r.get(0)?,
                        run_id: read_id(r, 1)?,
                        state: read_state(r, 2)?,
                        created_at: r.get(3)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn read_batch(row: &Row<'_>) -> rusqlite::Result<AsyncBatch> {
    Ok(AsyncBatch {
        batch_id: read_id(row, 0)?,
        prepared_state_id: read_id(row, 1)?,
        request_digest: row.get(2)?,
        concurrency: row.get(3)?,
        options_json: row.get(4)?,
        fake_binary: row.get(5)?,
    })
}

pub fn terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::NeedsAttention
    )
}

pub(super) fn insert_task_source(
    tx: &rusqlite::Transaction<'_>,
    run: &RunId,
    fallback: &PreparedStateId,
    source: Option<&SourceProvenance>,
) -> Result<()> {
    let state = source.map_or(fallback, |source| &source.prepared_state_id);
    let json = source
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| StoreError::InvalidRecord("invalid task source provenance"))?;
    tx.execute(
        "INSERT INTO batch_task_sources(run_id,prepared_state_id,provenance_json) VALUES(?1,?2,?3)",
        params![run.as_str(), state.as_str(), json],
    )?;
    Ok(())
}
