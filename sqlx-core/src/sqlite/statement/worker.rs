use crate::error::Error;
use crate::sqlite::statement::StatementHandle;
use crossbeam_channel::{unbounded, Sender};
use either::Either;
use futures_channel::oneshot;
use std::sync::{Arc, Weak};
use std::thread;

use libsqlite3_sys::{sqlite3_reset, sqlite3_step, SQLITE_DONE, SQLITE_OK, SQLITE_ROW};

// Each SQLite connection has a dedicated thread.

// TODO: Tweak this so that we can use a thread pool per pool of SQLite3 connections to reduce
//       OS resource usage. Low priority because a high concurrent load for SQLite3 is very
//       unlikely.

pub(crate) struct StatementWorker {
    tx: Sender<StatementWorkerCommand>,
}

enum StatementWorkerCommand {
    Step {
        statement: Weak<StatementHandle>,
        tx: oneshot::Sender<Result<Either<u64, ()>, Error>>,
    },
    Reset {
        statement: Weak<StatementHandle>,
        tx: oneshot::Sender<Result<(), Error>>,
    },
}

impl StatementWorker {
    pub(crate) fn new() -> Self {
        let (tx, rx) = unbounded();

        thread::spawn(move || {
            for cmd in rx {
                match cmd {
                    StatementWorkerCommand::Step { statement, tx } => {
                        let statement = if let Some(statement) = statement.upgrade() {
                            statement
                        } else {
                            // statement is already finalized, the sender shouldn't be expecting a response
                            continue;
                        };

                        // SAFETY: only the `StatementWorker` calls this function
                        let status = unsafe { sqlite3_step(statement.as_ptr()) };
                        let result = match status {
                            SQLITE_ROW => Ok(Either::Right(())),
                            SQLITE_DONE => Ok(Either::Left(statement.changes())),
                            _ => Err(statement.last_error().into()),
                        };

                        let _ = tx.send(result);
                    }
                    StatementWorkerCommand::Reset { statement, tx } => {
                        if let Some(statement) = statement.upgrade() {
                            // SAFETY: this must be the only place we call `sqlite3_reset`
                            let result = unsafe { sqlite3_reset(statement.as_ptr()) };

                            let _ = tx.send(if result == SQLITE_OK {
                                Ok(())
                            } else {
                                Err(statement.last_error().into())
                            });
                        }
                    }
                }
            }
        });

        Self { tx }
    }

    pub(crate) async fn step(
        &mut self,
        statement: &Arc<StatementHandle>,
    ) -> Result<Either<u64, ()>, Error> {
        let (tx, rx) = oneshot::channel();

        self.tx
            .send(StatementWorkerCommand::Step {
                statement: Arc::downgrade(statement),
                tx,
            })
            .map_err(|_| Error::WorkerCrashed)?;

        rx.await.map_err(|_| Error::WorkerCrashed)?
    }

    pub(crate) fn reset(&mut self, statement: &Arc<StatementHandle>) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();

        let _ = self
            .tx
            .send(StatementWorkerCommand::Reset {
                statement: Arc::downgrade(statement),
                tx,
            })
            .map_err(|_| Error::WorkerCrashed)?;

        rx.await.map_err(|_| Error::WorkerCrashed)?
    }
}
