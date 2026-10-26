use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, Authorization};
use tokio::sync::oneshot;

use crate::async_runtime::run_blocking;
use crate::config::LoadedConfig;
use crate::error::{DbmError, Result};
use crate::storage::{self, FinishedRunData, LockData, MetadataSnapshot, PlannedRunData, StepData};

use super::{BoxFuture, DatabaseAdapter, DatabaseInfo, DatabaseSession, SessionIntent};

pub(crate) struct SqliteAdapter;

impl DatabaseAdapter for SqliteAdapter {
    fn connect<'a>(
        &'a self,
        config: &'a LoadedConfig,
        intent: SessionIntent,
    ) -> BoxFuture<'a, Result<Box<dyn DatabaseSession>>> {
        Box::pin(async move {
            let target_identity = config
                .database_path
                .as_ref()
                .map(|path| path.display().to_string());
            let config = config.clone();
            let (connection, server_version) = run_blocking(move || {
                let connection = match intent {
                    SessionIntent::ReadOnly => storage::open_readonly(&config)?,
                    SessionIntent::ReadWrite => Some(storage::open_mutable(&config)?),
                };
                let server_version = match connection.as_ref() {
                    Some(connection) => {
                        connection.query_row("SELECT sqlite_version()", [], |row| {
                            row.get::<_, String>(0)
                        })?
                    }
                    None => "uninitialized".to_owned(),
                };
                Ok((connection, server_version))
            })
            .await?;

            let (sender, receiver) = mpsc::channel();
            let worker = thread::Builder::new()
                .name("dbm-sqlite-session".to_owned())
                .spawn(move || sqlite_worker(connection, intent, receiver))
                .map_err(|_| DbmError::Driver {
                    driver: "SQLite",
                    message: "could not start the dedicated database worker".to_owned(),
                })?;
            let worker_thread_id = format!("{:?}", worker.thread().id());

            Ok(Box::new(SqliteSession {
                sender: Some(sender),
                worker: Some(worker),
                info: DatabaseInfo {
                    dialect: "sqlite",
                    server_version,
                    database: None,
                    schema: Some("main".to_owned()),
                    target_identity,
                    session_id: Some(worker_thread_id),
                },
                intent,
                closed: false,
            }) as Box<dyn DatabaseSession>)
        })
    }
}

enum SqliteCommand {
    ReadSnapshot {
        namespace: String,
        reply: oneshot::Sender<Result<MetadataSnapshot>>,
    },
    InitializeMetadata {
        initialized_at: String,
        reply: oneshot::Sender<Result<()>>,
    },
    InsertPlannedRun {
        migration: crate::migration::Migration,
        run_id: String,
        batch_id: String,
        direction: String,
        created_at: String,
        plan_hash: String,
        reply: oneshot::Sender<Result<()>>,
    },
    MarkRunStarted {
        run_id: String,
        started_at: String,
        reply: oneshot::Sender<Result<()>>,
    },
    InsertStep {
        run_id: String,
        step_index: i64,
        step_key: String,
        direction: String,
        statement_hash: String,
        statement_kind: String,
        transaction_class: String,
        state: String,
        started_at: String,
        acknowledged_at: Option<String>,
        error_code: Option<String>,
        evidence: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    AcknowledgeStep {
        run_id: String,
        step_index: i64,
        acknowledged_at: String,
        reply: oneshot::Sender<Result<()>>,
    },
    MarkStepIndeterminate {
        run_id: String,
        step_index: i64,
        error_code: String,
        evidence: String,
        reply: oneshot::Sender<Result<()>>,
    },
    InsertApplied {
        migration: crate::migration::Migration,
        applied_at: String,
        reply: oneshot::Sender<Result<()>>,
    },
    DeleteApplied {
        migration: crate::migration::Migration,
        reply: oneshot::Sender<Result<()>>,
    },
    FinishRun {
        run_id: String,
        state: String,
        ended_at: String,
        duration_ms: i64,
        error_code: Option<String>,
        error_summary_redacted: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    RecordLock {
        owner_id: String,
        run_id: String,
        acquired_at: String,
        reply: oneshot::Sender<Result<i64>>,
    },
    ReleaseRecordedLock {
        owner_id: String,
        released_at: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Verify {
        reply: oneshot::Sender<Result<()>>,
    },
    Begin {
        reply: oneshot::Sender<Result<()>>,
    },
    Execute {
        sql: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Commit {
        reply: oneshot::Sender<Result<()>>,
    },
    Rollback {
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
}

struct SqliteSession {
    sender: Option<Sender<SqliteCommand>>,
    worker: Option<JoinHandle<()>>,
    info: DatabaseInfo,
    intent: SessionIntent,
    closed: bool,
}

impl SqliteSession {
    async fn request<T, F>(&self, make_command: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(oneshot::Sender<Result<T>>) -> SqliteCommand,
    {
        let (reply, response) = oneshot::channel();
        let sender = self.sender.as_ref().ok_or_else(worker_closed_error)?;
        sender
            .send(make_command(reply))
            .map_err(|_| worker_closed_error())?;
        response.await.map_err(|_| worker_closed_error())?
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.intent == SessionIntent::ReadOnly {
            return Err(DbmError::Safety(
                "a read-only SQLite session cannot execute mutations".to_owned(),
            ));
        }
        if self.closed {
            return Err(worker_closed_error());
        }
        Ok(())
    }
}

impl DatabaseSession for SqliteSession {
    fn info(&self) -> &DatabaseInfo {
        &self.info
    }

    fn read_snapshot<'a>(
        &'a mut self,
        namespace: &'a str,
    ) -> BoxFuture<'a, Result<MetadataSnapshot>> {
        Box::pin(async move {
            let namespace = namespace.to_owned();
            self.request(|reply| SqliteCommand::ReadSnapshot { namespace, reply })
                .await
        })
    }

    fn initialize_metadata<'a>(&'a mut self, initialized_at: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let initialized_at = initialized_at.to_owned();
            self.request(|reply| SqliteCommand::InitializeMetadata {
                initialized_at,
                reply,
            })
            .await
        })
    }

    fn insert_planned_run<'a>(
        &'a mut self,
        migration: &'a crate::migration::Migration,
        run: PlannedRunData<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let migration = migration.clone();
            let run_id = run.run_id.to_owned();
            let batch_id = run.batch_id.to_owned();
            let direction = run.direction.to_owned();
            let created_at = run.created_at.to_owned();
            let plan_hash = run.plan_hash.to_owned();
            self.request(|reply| SqliteCommand::InsertPlannedRun {
                migration,
                run_id,
                batch_id,
                direction,
                created_at,
                plan_hash,
                reply,
            })
            .await
        })
    }

    fn mark_run_started<'a>(
        &'a mut self,
        run_id: &'a str,
        started_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let run_id = run_id.to_owned();
            let started_at = started_at.to_owned();
            self.request(|reply| SqliteCommand::MarkRunStarted {
                run_id,
                started_at,
                reply,
            })
            .await
        })
    }

    fn insert_step<'a>(&'a mut self, step: StepData<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let run_id = step.run_id.to_owned();
            let step_key = step.step_key.to_owned();
            let direction = step.direction.to_owned();
            let statement_hash = step.statement_hash.to_owned();
            let statement_kind = step.statement_kind.to_owned();
            let transaction_class = step.transaction_class.to_owned();
            let state = step.state.to_owned();
            let started_at = step.started_at.to_owned();
            let acknowledged_at = step.acknowledged_at.map(str::to_owned);
            let error_code = step.error_code.map(str::to_owned);
            let evidence = step.evidence.map(str::to_owned);
            self.request(|reply| SqliteCommand::InsertStep {
                run_id,
                step_index: step.step_index,
                step_key,
                direction,
                statement_hash,
                statement_kind,
                transaction_class,
                state,
                started_at,
                acknowledged_at,
                error_code,
                evidence,
                reply,
            })
            .await
        })
    }

    fn acknowledge_step<'a>(
        &'a mut self,
        run_id: &'a str,
        step_index: i64,
        acknowledged_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let run_id = run_id.to_owned();
            let acknowledged_at = acknowledged_at.to_owned();
            self.request(|reply| SqliteCommand::AcknowledgeStep {
                run_id,
                step_index,
                acknowledged_at,
                reply,
            })
            .await
        })
    }

    fn mark_step_indeterminate<'a>(
        &'a mut self,
        run_id: &'a str,
        step_index: i64,
        error_code: &'a str,
        evidence: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let run_id = run_id.to_owned();
            let error_code = error_code.to_owned();
            let evidence = evidence.to_owned();
            self.request(|reply| SqliteCommand::MarkStepIndeterminate {
                run_id,
                step_index,
                error_code,
                evidence,
                reply,
            })
            .await
        })
    }

    fn insert_applied<'a>(
        &'a mut self,
        migration: &'a crate::migration::Migration,
        applied_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let migration = migration.clone();
            let applied_at = applied_at.to_owned();
            self.request(|reply| SqliteCommand::InsertApplied {
                migration,
                applied_at,
                reply,
            })
            .await
        })
    }

    fn delete_applied<'a>(
        &'a mut self,
        migration: &'a crate::migration::Migration,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let migration = migration.clone();
            self.request(|reply| SqliteCommand::DeleteApplied { migration, reply })
                .await
        })
    }

    fn finish_run<'a>(&'a mut self, run: FinishedRunData<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let run_id = run.run_id.to_owned();
            let state = run.state.to_owned();
            let ended_at = run.ended_at.to_owned();
            let error_code = run.error_code.map(str::to_owned);
            let error_summary_redacted = run.error_summary_redacted.map(str::to_owned);
            self.request(|reply| SqliteCommand::FinishRun {
                run_id,
                state,
                ended_at,
                duration_ms: run.duration_ms,
                error_code,
                error_summary_redacted,
                reply,
            })
            .await
        })
    }

    fn record_lock<'a>(&'a mut self, lock: LockData<'a>) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let owner_id = lock.owner_id.to_owned();
            let run_id = lock.run_id.to_owned();
            let acquired_at = lock.now.to_owned();
            self.request(|reply| SqliteCommand::RecordLock {
                owner_id,
                run_id,
                acquired_at,
                reply,
            })
            .await
        })
    }

    fn release_recorded_lock<'a>(
        &'a mut self,
        owner_id: &'a str,
        released_at: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let owner_id = owner_id.to_owned();
            let released_at = released_at.to_owned();
            self.request(|reply| SqliteCommand::ReleaseRecordedLock {
                owner_id,
                released_at,
                reply,
            })
            .await
        })
    }

    fn verify_session<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Err(worker_closed_error());
            }
            self.request(|reply| SqliteCommand::Verify { reply }).await
        })
    }

    fn acquire_lock<'a>(&'a mut self, _timeout: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            Err(DbmError::Unsupported(
                "SQLite does not provide a session advisory lock; use its transaction-bound writer lock"
                    .to_owned(),
            ))
        })
    }

    fn release_lock<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            Err(DbmError::Unsupported(
                "SQLite does not provide a session advisory lock".to_owned(),
            ))
        })
    }

    fn begin<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            self.request(|reply| SqliteCommand::Begin { reply }).await
        })
    }

    fn execute_statement<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            let sql = sql.to_owned();
            self.request(|reply| SqliteCommand::Execute { sql, reply })
                .await
        })
    }

    fn execute_nontransactional_statement<'a>(
        &'a mut self,
        _sql: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(DbmError::Unsupported(
                "SQLite non-transactional migration execution is not supported".to_owned(),
            ))
        })
    }

    fn commit<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            self.request(|reply| SqliteCommand::Commit { reply }).await
        })
    }

    fn rollback<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_writable()?;
            self.request(|reply| SqliteCommand::Rollback { reply })
                .await
        })
    }

    fn close<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Ok(());
            }
            let close_result = self.request(|reply| SqliteCommand::Close { reply }).await;
            self.sender.take();
            let join_result = match self.worker.take() {
                Some(worker) => {
                    run_blocking(move || {
                        worker.join().map_err(|_| DbmError::Driver {
                            driver: "SQLite",
                            message: "dedicated database worker terminated unexpectedly".to_owned(),
                        })
                    })
                    .await
                }
                None => Ok(()),
            };
            self.closed = true;
            match (close_result, join_result) {
                (Err(error), _) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Ok(()), Ok(())) => Ok(()),
            }
        })
    }
}

fn sqlite_worker(
    mut connection: Option<Connection>,
    intent: SessionIntent,
    receiver: Receiver<SqliteCommand>,
) {
    let mut transaction_open = false;
    while let Ok(command) = receiver.recv() {
        match command {
            SqliteCommand::ReadSnapshot { namespace, reply } => {
                let result = match connection.as_ref() {
                    Some(connection) => {
                        storage::read_snapshot_from_connection(connection, &namespace)
                    }
                    None => Ok(MetadataSnapshot::default()),
                };
                let _ = reply.send(result);
            }
            SqliteCommand::InitializeMetadata {
                initialized_at,
                reply,
            } => {
                let result = if intent == SessionIntent::ReadOnly {
                    Err(DbmError::Safety(
                        "a read-only SQLite session cannot initialize metadata".to_owned(),
                    ))
                } else if !transaction_open {
                    Err(DbmError::Safety(
                        "SQLite metadata initialization requires an open transaction".to_owned(),
                    ))
                } else {
                    match connection.as_ref() {
                        Some(connection) => storage::ensure_schema(connection, &initialized_at),
                        None => Err(worker_closed_error()),
                    }
                };
                let _ = reply.send(result);
            }
            SqliteCommand::InsertPlannedRun {
                migration,
                run_id,
                batch_id,
                direction,
                created_at,
                plan_hash,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| {
                        storage::insert_planned_run(
                            connection,
                            &migration,
                            PlannedRunData {
                                run_id: &run_id,
                                batch_id: &batch_id,
                                direction: &direction,
                                created_at: &created_at,
                                plan_hash: &plan_hash,
                            },
                        )
                    },
                );
                let _ = reply.send(result);
            }
            SqliteCommand::MarkRunStarted {
                run_id,
                started_at,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| storage::mark_run_started(connection, &run_id, &started_at),
                );
                let _ = reply.send(result);
            }
            SqliteCommand::InsertStep {
                run_id,
                step_index,
                step_key,
                direction,
                statement_hash,
                statement_kind,
                transaction_class,
                state,
                started_at,
                acknowledged_at,
                error_code,
                evidence,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| {
                        storage::insert_step(
                            connection,
                            StepData {
                                run_id: &run_id,
                                step_index,
                                step_key: &step_key,
                                direction: &direction,
                                statement_hash: &statement_hash,
                                statement_kind: &statement_kind,
                                transaction_class: &transaction_class,
                                state: &state,
                                started_at: &started_at,
                                acknowledged_at: acknowledged_at.as_deref(),
                                error_code: error_code.as_deref(),
                                evidence: evidence.as_deref(),
                            },
                        )
                    },
                );
                let _ = reply.send(result);
            }
            SqliteCommand::AcknowledgeStep {
                run_id,
                step_index,
                acknowledged_at,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| {
                        storage::acknowledge_step(connection, &run_id, step_index, &acknowledged_at)
                    },
                );
                let _ = reply.send(result);
            }
            SqliteCommand::MarkStepIndeterminate {
                run_id,
                step_index,
                error_code,
                evidence,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| {
                        storage::mark_step_indeterminate(
                            connection,
                            &run_id,
                            step_index,
                            &error_code,
                            &evidence,
                        )
                    },
                );
                let _ = reply.send(result);
            }
            SqliteCommand::InsertApplied {
                migration,
                applied_at,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| storage::insert_applied(connection, &migration, &applied_at),
                );
                let _ = reply.send(result);
            }
            SqliteCommand::DeleteApplied { migration, reply } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| storage::delete_applied(connection, &migration),
                );
                let _ = reply.send(result);
            }
            SqliteCommand::FinishRun {
                run_id,
                state,
                ended_at,
                duration_ms,
                error_code,
                error_summary_redacted,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| {
                        storage::finish_run(
                            connection,
                            FinishedRunData {
                                run_id: &run_id,
                                state: &state,
                                ended_at: &ended_at,
                                duration_ms,
                                error_code: error_code.as_deref(),
                                error_summary_redacted: error_summary_redacted.as_deref(),
                            },
                        )
                    },
                );
                let _ = reply.send(result);
            }
            SqliteCommand::RecordLock {
                owner_id,
                run_id,
                acquired_at,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| {
                        storage::acquire_lock(
                            connection,
                            LockData {
                                owner_id: &owner_id,
                                run_id: &run_id,
                                now: &acquired_at,
                            },
                        )
                    },
                );
                let _ = reply.send(result);
            }
            SqliteCommand::ReleaseRecordedLock {
                owner_id,
                released_at,
                reply,
            } => {
                let result = execute_metadata_write(
                    connection.as_ref(),
                    intent,
                    transaction_open,
                    |connection| storage::release_lock(connection, &owner_id, &released_at),
                );
                let _ = reply.send(result);
            }
            SqliteCommand::Verify { reply } => {
                let result = match connection.as_ref() {
                    Some(connection) => connection
                        .query_row("SELECT 1", [], |_| Ok(()))
                        .map_err(DbmError::from),
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
            SqliteCommand::Begin { reply } => {
                let result = if intent == SessionIntent::ReadOnly {
                    Err(DbmError::Safety(
                        "a read-only SQLite session cannot execute mutations".to_owned(),
                    ))
                } else if transaction_open {
                    Err(DbmError::Safety(
                        "SQLite session already has an open transaction".to_owned(),
                    ))
                } else {
                    match connection.as_ref() {
                        Some(connection) => connection
                            .execute_batch("BEGIN IMMEDIATE")
                            .map_err(DbmError::from),
                        None => Err(worker_closed_error()),
                    }
                };
                if result.is_ok() {
                    transaction_open = true;
                }
                let _ = reply.send(result);
            }
            SqliteCommand::Execute { sql, reply } => {
                let result = if intent == SessionIntent::ReadOnly {
                    Err(DbmError::Safety(
                        "a read-only SQLite session cannot execute mutations".to_owned(),
                    ))
                } else if !transaction_open {
                    Err(DbmError::Safety(
                        "SQLite migration statements require an open managed transaction"
                            .to_owned(),
                    ))
                } else {
                    match connection.as_ref() {
                        Some(connection) => execute_managed_sql(connection, &sql),
                        None => Err(worker_closed_error()),
                    }
                };
                let _ = reply.send(result);
            }
            SqliteCommand::Commit { reply } => {
                let result = if !transaction_open {
                    Err(DbmError::Safety(
                        "SQLite session has no open transaction to commit".to_owned(),
                    ))
                } else {
                    match connection.as_ref() {
                        Some(connection) => connection.execute_batch("COMMIT").map_err(|error| {
                            DbmError::ReconciliationRequired(format!(
                                "SQLite transaction commit has an uncertain result: {error}"
                            ))
                        }),
                        None => Err(worker_closed_error()),
                    }
                };
                if result.is_ok() {
                    transaction_open = false;
                }
                let _ = reply.send(result);
            }
            SqliteCommand::Rollback { reply } => {
                let result = if !transaction_open {
                    Err(DbmError::Safety(
                        "SQLite session has no open transaction to roll back".to_owned(),
                    ))
                } else {
                    match connection.as_ref() {
                        Some(connection) => connection.execute_batch("ROLLBACK").map_err(|error| {
                            DbmError::ReconciliationRequired(format!(
                                "SQLite transaction rollback could not be confirmed: {error}"
                            ))
                        }),
                        None => Err(worker_closed_error()),
                    }
                };
                if result.is_ok() {
                    transaction_open = false;
                }
                let _ = reply.send(result);
            }
            SqliteCommand::Close { reply } => {
                let result = close_connection(&mut connection, transaction_open);
                let _ = reply.send(result);
                return;
            }
        }
    }

    if transaction_open {
        let _ = close_connection(&mut connection, true);
    }
}

fn execute_metadata_write<T, F>(
    connection: Option<&Connection>,
    intent: SessionIntent,
    transaction_open: bool,
    operation: F,
) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    if intent == SessionIntent::ReadOnly {
        return Err(DbmError::Safety(
            "a read-only SQLite session cannot write migration metadata".to_owned(),
        ));
    }
    if !transaction_open {
        return Err(DbmError::Safety(
            "SQLite migration metadata writes require an open transaction".to_owned(),
        ));
    }
    let connection = connection.ok_or_else(worker_closed_error)?;
    operation(connection)
}

fn execute_managed_sql(connection: &Connection, sql: &str) -> Result<()> {
    connection.authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
        if forbidden_action(context.action) {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))?;
    let result = connection
        .execute_batch(sql)
        .map_err(classify_managed_sql_error);
    let clear_result = connection.authorizer(Some(|_: rusqlite::hooks::AuthContext<'_>| {
        Authorization::Allow
    }));
    clear_result.map_err(DbmError::from)?;
    result
}

fn classify_managed_sql_error(error: rusqlite::Error) -> DbmError {
    match error {
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            DbmError::Lock(
                "SQLite writer lock was unavailable during migration execution".to_owned(),
            )
        }
        rusqlite::Error::SqliteFailure(failure, _) => DbmError::Execution(format!(
            "SQLite rejected a managed migration statement with extended result code {}",
            failure.extended_code
        )),
        other => DbmError::Database(other),
    }
}

fn forbidden_action(action: AuthAction<'_>) -> bool {
    match action {
        AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::Pragma { .. } => true,
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::CreateTable { table_name }
        | AuthAction::CreateTempTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::DropTempTable { table_name }
        | AuthAction::CreateIndex { table_name, .. }
        | AuthAction::CreateTempIndex { table_name, .. }
        | AuthAction::DropIndex { table_name, .. }
        | AuthAction::DropTempIndex { table_name, .. }
        | AuthAction::CreateTrigger { table_name, .. }
        | AuthAction::CreateTempTrigger { table_name, .. }
        | AuthAction::DropTrigger { table_name, .. }
        | AuthAction::DropTempTrigger { table_name, .. }
        | AuthAction::AlterTable { table_name, .. }
        | AuthAction::Update { table_name, .. } => is_metadata_table(table_name),
        _ => false,
    }
}

fn is_metadata_table(table_name: &str) -> bool {
    [
        "schema_migration_meta",
        "schema_migrations",
        "schema_migration_runs",
        "schema_migration_steps",
        "schema_migration_locks",
    ]
    .iter()
    .any(|name| table_name.eq_ignore_ascii_case(name))
}

fn close_connection(connection: &mut Option<Connection>, transaction_open: bool) -> Result<()> {
    let Some(connection) = connection.take() else {
        return Ok(());
    };
    if transaction_open {
        connection.execute_batch("ROLLBACK").map_err(|error| {
            DbmError::ReconciliationRequired(format!(
                "SQLite session closed with an unconfirmed rollback: {error}"
            ))
        })?;
    }
    Ok(())
}

fn worker_closed_error() -> DbmError {
    DbmError::Driver {
        driver: "SQLite",
        message: "dedicated database worker is unavailable".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionIntent, SqliteAdapter};
    use crate::config;
    use crate::database::DatabaseAdapter;
    use crate::identity::MigrationIdentity;
    use crate::migration::{
        Migration, MigrationFormat, MigrationKind, SqlDialect, TransactionPolicy,
    };
    use crate::storage::{FinishedRunData, LockData, PlannedRunData, StepData};

    fn sample_migration() -> Migration {
        Migration {
            identity: MigrationIdentity {
                namespace: "default".to_owned(),
                version: "20261024120000000000".to_owned(),
            },
            kind: MigrationKind::Versioned,
            description: "create_probe".to_owned(),
            filename: "20261024120000000000_create_probe.sql".to_owned(),
            source_path: "migrations/versioned/20261024120000000000_create_probe.sql".to_owned(),
            checksum: "sha256:test-checksum".to_owned(),
            up_sql: "CREATE TABLE adapter_probe (id INTEGER PRIMARY KEY);".to_owned(),
            down_sql: Some("DROP TABLE adapter_probe;".to_owned()),
            dialect: SqlDialect::Sqlite,
            format: MigrationFormat::Single,
            transaction: TransactionPolicy::Always,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_session_is_owned_by_a_dedicated_worker_thread() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("dbm.toml");
        config::initialize(&config_path).expect("initialize test project");
        let config = config::load(&config_path).expect("load test config");
        let mut session = SqliteAdapter
            .connect(&config, SessionIntent::ReadOnly)
            .await
            .expect("connect read-only SQLite session");

        let executor_thread = format!("{:?}", std::thread::current().id());
        assert_ne!(
            session.info().session_id.as_deref(),
            Some(executor_thread.as_str())
        );
        session
            .verify_session()
            .await
            .expect("dedicated worker verifies its session");
        session.close().await.expect("close SQLite session");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_metadata_bootstrap_requires_and_uses_the_writer_transaction() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("dbm.toml");
        config::initialize(&config_path).expect("initialize test project");
        let config = config::load(&config_path).expect("load test config");
        let mut session = SqliteAdapter
            .connect(&config, SessionIntent::ReadWrite)
            .await
            .expect("connect mutable SQLite session");

        assert!(
            session
                .initialize_metadata("2026-10-24T12:00:00Z")
                .await
                .is_err(),
            "metadata bootstrap must require an open writer transaction"
        );
        session.begin().await.expect("begin writer transaction");
        session
            .initialize_metadata("2026-10-24T12:00:00Z")
            .await
            .expect("bootstrap metadata inside the transaction");
        session.commit().await.expect("commit metadata bootstrap");

        let migration = sample_migration();
        session
            .begin()
            .await
            .expect("begin planned-run transaction");
        session
            .insert_planned_run(
                &migration,
                PlannedRunData {
                    run_id: "adapter-run",
                    batch_id: "adapter-batch",
                    direction: "up",
                    created_at: "2026-10-24T12:00:00.000000Z",
                    plan_hash: "sha256:adapter-plan",
                },
            )
            .await
            .expect("persist planned run through the adapter");
        session
            .commit()
            .await
            .expect("commit planned-run transaction");

        session
            .begin()
            .await
            .expect("begin lock metadata transaction");
        assert_eq!(
            session
                .record_lock(LockData {
                    owner_id: "adapter-owner",
                    run_id: "adapter-run",
                    now: "2026-10-24T12:00:00.500000Z",
                })
                .await
                .expect("record SQLite writer-lock metadata"),
            1
        );
        session.commit().await.expect("commit SQLite lock metadata");

        session.begin().await.expect("begin run-start transaction");
        session
            .mark_run_started("adapter-run", "2026-10-24T12:00:01.000000Z")
            .await
            .expect("persist started run through the adapter");
        session
            .commit()
            .await
            .expect("commit run-start transaction");

        session
            .begin()
            .await
            .expect("begin migration execution transaction");
        session
            .insert_step(StepData {
                run_id: "adapter-run",
                step_index: 0,
                step_key: "adapter-step-0",
                direction: "up",
                statement_hash: "sha256:adapter-statement",
                statement_kind: "CREATE",
                transaction_class: "transactional",
                state: "intent_recorded",
                started_at: "2026-10-24T12:00:02.000000Z",
                acknowledged_at: None,
                error_code: None,
                evidence: None,
            })
            .await
            .expect("persist step intent through the adapter");
        session
            .execute_statement(&migration.up_sql)
            .await
            .expect("execute migration SQL through the adapter");
        session
            .acknowledge_step("adapter-run", 0, "2026-10-24T12:00:03.000000Z")
            .await
            .expect("acknowledge executed step through the adapter");
        session
            .insert_applied(&migration, "2026-10-24T12:00:03.000000Z")
            .await
            .expect("persist applied migration through the adapter");
        session
            .finish_run(FinishedRunData {
                run_id: "adapter-run",
                state: "succeeded",
                ended_at: "2026-10-24T12:00:03.000000Z",
                duration_ms: 1,
                error_code: None,
                error_summary_redacted: None,
            })
            .await
            .expect("persist successful run through the adapter");
        session
            .commit()
            .await
            .expect("commit schema and metadata together");

        session
            .begin()
            .await
            .expect("begin lock metadata release transaction");
        session
            .release_recorded_lock("adapter-owner", "2026-10-24T12:00:04.000000Z")
            .await
            .expect("record SQLite writer-lock release");
        session
            .commit()
            .await
            .expect("commit SQLite lock metadata release");

        let snapshot = session
            .read_snapshot("default")
            .await
            .expect("read newly initialized metadata");
        assert!(snapshot.initialized);
        assert_eq!(snapshot.applied.len(), 1);
        assert_eq!(snapshot.applied[0].version, migration.identity.version);
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].state, "succeeded");
        assert_eq!(snapshot.steps.len(), 1);
        assert_eq!(snapshot.steps[0].state, "acknowledged");
        assert_eq!(
            snapshot.lock.as_ref().map(|lock| lock.state.as_str()),
            Some("released")
        );
        session.close().await.expect("close SQLite session");
    }
}
