use std::path::Path;
use std::str::FromStr;
#[cfg(feature = "mwal_backend")]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam::channel::RecvTimeoutError;
use rusqlite::{params_from_iter, OpenFlags};
use tokio::sync::oneshot;
use tracing::warn;

use crate::libsql::wal_hook::WalHook;
use crate::libsql::WalConnection;
use crate::query::{
    Column, ErrorCode, QueryError, QueryResponse, QueryResult, ResultSet, Row, Value,
};
use crate::query_analysis::{State, Statement};

use super::{Database, TXN_TIMEOUT_SECS};

#[derive(Clone)]
pub struct LibSqlDb {
    sender: crossbeam::channel::Sender<(Statement, Vec<Value>, oneshot::Sender<QueryResult>)>,
}

fn execute_query(conn: &rusqlite::Connection, stmt: &Statement, params: Vec<Value>) -> QueryResult {
    let mut rows = vec![];
    let mut prepared = conn.prepare(&stmt.stmt)?;
    let columns = prepared
        .columns()
        .iter()
        .map(|col| Column {
            name: col.name().into(),
            ty: col
                .decl_type()
                .map(FromStr::from_str)
                .transpose()
                .ok()
                .flatten(),
        })
        .collect::<Vec<_>>();
    let mut qresult = prepared.query(params_from_iter(
        params.into_iter().map(rusqlite::types::Value::from),
    ))?;
    while let Some(row) = qresult.next()? {
        let mut values = vec![];
        for (i, _) in columns.iter().enumerate() {
            values.push(row.get::<usize, rusqlite::types::Value>(i)?.into());
        }
        rows.push(Row { values });
    }

    Ok(QueryResponse::ResultSet(ResultSet { columns, rows }))
}

fn rollback(conn: &rusqlite::Connection) {
    conn.execute("rollback transaction;", ())
        .expect("failed to rollback");
}

macro_rules! ok_or_exit {
    ($e:expr) => {
        if let Err(_) = $e {
            return;
        }
    };
}

fn open_db(
    path: impl AsRef<Path> + Send + 'static,
    #[cfg(feature = "mwal_backend")] vwal_methods: Option<
        Arc<Mutex<mwal::ffi::libsql_wal_methods>>,
    >,
    wal_hook: impl WalHook + Send + Clone + 'static,
) -> anyhow::Result<WalConnection> {
    let mut retries = 0;
    loop {
        #[cfg(feature = "mwal_backend")]
        let conn_result = match vwal_methods {
            Some(ref vwal_methods) => crate::libsql::mwal::open_with_virtual_wal(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                vwal_methods.clone(),
            ),
            None => crate::libsql::open_with_regular_wal(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                wal_hook.clone(),
            ),
        };
        #[cfg(not(feature = "mwal_backend"))]
        let conn_result = crate::libsql::open_with_regular_wal(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            wal_hook.clone(),
        );

        match conn_result {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                match e.downcast::<rusqlite::Error>() {
                    // > When the last connection to a particular database is closing, that
                    // > connection will acquire an exclusive lock for a short time while it cleans
                    // > up the WAL and shared-memory files. If a second database tries to open and
                    // > query the database while the first connection is still in the middle of its
                    // > cleanup process, the second connection might get an SQLITE_BUSY error.
                    //
                    // For this reason we may not be able to open the database right away, so we
                    // retry a couple of times before giving up.
                    Ok(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == rusqlite::ffi::ErrorCode::DatabaseBusy && retries < 10 =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                        retries += 1;
                    }
                    Ok(e) => panic!("Unhandled error opening libsql: {}", e),
                    Err(e) => panic!("Unhandled error opening libsql: {}", e),
                }
            }
        }
    }
}

impl LibSqlDb {
    pub fn new(
        path: impl AsRef<Path> + Send + 'static,
        #[cfg(feature = "mwal_backend")] vwal_methods: Option<
            Arc<Mutex<mwal::ffi::libsql_wal_methods>>,
        >,
        wal_hook: impl WalHook + Send + Clone + 'static,
    ) -> anyhow::Result<Self> {
        let (sender, receiver) =
            crossbeam::channel::unbounded::<(Statement, Vec<Value>, oneshot::Sender<QueryResult>)>(
            );

        tokio::task::spawn_blocking(move || {
            let conn = open_db(
                path,
                #[cfg(feature = "mwal_backend")]
                vwal_methods,
                wal_hook,
            )
            .unwrap();
            let mut state = State::Start;
            let mut timeout_deadline = None;
            let mut timedout = false;
            loop {
                let (stmt, params, sender) = match timeout_deadline {
                    Some(deadline) => match receiver.recv_deadline(deadline) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => {
                            warn!("transaction timed out");
                            rollback(&conn);
                            timeout_deadline = None;
                            timedout = true;
                            state = State::Start;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match receiver.recv() {
                        Ok(msg) => msg,
                        Err(_) => break,
                    },
                };

                if !timedout {
                    let old_state = state;
                    let result = execute_query(&conn, &stmt, params);
                    if result.is_ok() {
                        state.step(stmt.kind);
                        match (old_state, state) {
                            (State::Start, State::TxnOpened) => {
                                timeout_deadline.replace(
                                    Instant::now() + Duration::from_secs(TXN_TIMEOUT_SECS),
                                );
                            }
                            (State::TxnOpened, State::TxnClosed) => {
                                timeout_deadline.take();
                                state.reset();
                            }
                            (_, State::Invalid) => panic!("invalid state"),
                            _ => (),
                        }
                    }
                    ok_or_exit!(sender.send(result));
                } else {
                    ok_or_exit!(sender.send(Err(QueryError::new(
                        ErrorCode::TxTimeout,
                        "transaction timedout",
                    ))));
                    timedout = false;
                }
            }
        });

        Ok(Self { sender })
    }
}

#[async_trait::async_trait]
impl Database for LibSqlDb {
    async fn execute(&self, query: Statement, params: Vec<Value>) -> QueryResult {
        let (sender, receiver) = oneshot::channel();
        let _ = self.sender.send((query, params, sender));
        receiver
            .await
            .map_err(|e| QueryError::new(ErrorCode::Internal, e.to_string()))?
    }
}
