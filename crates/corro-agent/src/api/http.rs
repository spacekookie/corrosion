use std::{
    cmp,
    time::{Duration, Instant},
};

use axum::Extension;
use bb8::RunError;
use compact_str::{CompactString, ToCompactString};
use corro_types::{
    agent::{Agent, KnownDbVersion},
    api::{QueryResultBuilder, RqliteResponse, RqliteResult, Statement},
    broadcast::{Changeset, Timestamp},
    schema::{make_schema_inner, parse_sql},
    sqlite::SqlitePool,
};
use hyper::StatusCode;
use rusqlite::{params, params_from_iter, ToSql, Transaction};
use tokio::task::block_in_place;
use tracing::{error, info, trace};

use corro_types::{
    broadcast::{BroadcastInput, Message, MessageV1},
    change::Change,
};

use crate::agent::process_subs;

// TODO: accept a few options
// #[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
// #[serde(rename_all = "snake_case")]
// pub struct RqliteRequestOptions {
//     pretty: Option<bool>,
//     timings: Option<bool>,
//     transaction: Option<bool>,
//     q: Option<String>,
// }

#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    #[error("could not acquire pooled connection: {0}")]
    ConnAcquisition(#[from] RunError<bb8_rusqlite::Error>),
    #[error("rusqlite: {0}")]
    Rusqlite(#[from] rusqlite::Error),
    #[error("too many rows impacted")]
    TooManyRowsImpacted,
}

pub async fn make_broadcastable_changes<F, T>(
    agent: &Agent,
    f: F,
) -> Result<(T, Duration), ChangeError>
where
    F: Fn(&Transaction) -> Result<T, ChangeError>,
{
    trace!("getting conn...");
    let mut conn = agent.read_write_pool().get().await?;
    trace!("got conn");

    let actor_id = agent.actor_id();

    let start = Instant::now();
    block_in_place(move || {
        let tx = conn.transaction()?;

        let start_version: i64 = tx
            .prepare_cached("SELECT crsql_dbversion();")?
            .query_row((), |row| row.get(0))?;

        let ret = f(&tx)?;

        let rows_impacted: i64 = tx
            .prepare_cached("SELECT crsql_rows_impacted()")?
            .query_row((), |row| row.get(0))?;

        if rows_impacted > agent.config().max_change_size {
            return Err(ChangeError::TooManyRowsImpacted);
        }

        let booked = agent.bookie().for_actor(actor_id);
        let elapsed = {
            let mut book_writer = booked.write();

            let last_version = book_writer.last().unwrap_or(0);
            trace!("last_version: {last_version}");
            let version = last_version + 1;
            trace!("version: {version}");

            let (changes, db_version) = {
                let mut prepped = tx.prepare_cached(r#"SELECT "table", pk, cid, val, col_version, db_version FROM crsql_changes WHERE site_id IS NULL AND db_version > ?"#)?;

                let mut end_version = start_version;

                let mapped = prepped.query_map([start_version], |row| {
                    let change = Change {
                        table: row.get(0)?,
                        pk: row.get(1)?,
                        cid: row.get(2)?,
                        val: row.get(3)?,
                        col_version: row.get(4)?,
                        db_version: row.get(5)?,
                        site_id: actor_id.to_bytes(),
                    };
                    end_version = cmp::max(end_version, change.db_version);
                    Ok(change)
                })?;

                let changes = mapped.collect::<Result<Vec<Change>, rusqlite::Error>>()?;

                let db_version = if end_version > start_version {
                    tx.prepare_cached(
                        r#"
                        INSERT INTO __corro_bookkeeping (actor_id, start_version, db_version, ts)
                            VALUES (?, ?, ?, ?);
                    "#,
                    )?
                    .execute(params![
                        actor_id,
                        version,
                        end_version,
                        Timestamp::from(agent.clock().new_timestamp())
                    ])?;
                    Some(end_version)
                } else {
                    None
                };

                (changes, db_version)
            };

            tx.commit()?;
            let elapsed = start.elapsed();

            if !changes.is_empty() {
                let ts: Timestamp = agent.clock().new_timestamp().into();
                book_writer.insert(
                    version,
                    match db_version {
                        Some(db_version) => KnownDbVersion::Current { db_version, ts },
                        None => KnownDbVersion::Cleared,
                    },
                );

                if let Some(db_version) = db_version {
                    process_subs(agent, &changes, db_version);
                }

                let tx_bcast = agent.tx_bcast().clone();
                tokio::spawn(async move {
                    if let Err(e) = tx_bcast
                        .send(BroadcastInput::AddBroadcast(Message::V1(
                            MessageV1::Change {
                                actor_id,
                                version,
                                changeset: Changeset::Full { changes, ts },
                            },
                        )))
                        .await
                    {
                        error!("could not send change message for broadcast: {e}");
                    }
                });
            }
            elapsed
        };

        Ok::<_, ChangeError>((ret, elapsed))
    })
}

fn execute_statement(tx: &Transaction, stmt: &Statement) -> rusqlite::Result<usize> {
    match stmt {
        Statement::Simple(q) => tx.execute(&q, []),
        Statement::WithParams(params) => {
            let mut params = params.into_iter();

            let first = params.next();
            match first.as_ref().and_then(|q| q.as_str()) {
                Some(q) => tx.execute(&q, params_from_iter(params)),
                None => Ok(0),
            }
        }
        Statement::WithNamedParams(q, params) => tx.execute(
            &q,
            params
                .iter()
                .map(|(k, v)| (k.as_str(), v as &dyn ToSql))
                .collect::<Vec<(&str, &dyn ToSql)>>()
                .as_slice(),
        ),
    }
}

pub async fn api_v1_db_execute(
    // axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Extension(agent): Extension<Agent>,
    axum::extract::Json(statements): axum::extract::Json<Vec<Statement>>,
) -> (StatusCode, axum::Json<RqliteResponse>) {
    if statements.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: "at least 1 statement is required".into(),
                }],
                time: None,
            }),
        );
    }

    let res = make_broadcastable_changes(&agent, move |tx| {
        let mut total_rows_affected = 0;

        let results = statements
            .iter()
            .filter_map(|stmt| {
                let start = Instant::now();
                let res = execute_statement(&tx, stmt);

                Some(match res {
                    Ok(rows_affected) => {
                        total_rows_affected += rows_affected;
                        RqliteResult::Execute {
                            rows_affected,
                            time: Some(start.elapsed().as_secs_f64()),
                        }
                    }
                    Err(e) => RqliteResult::Error {
                        error: e.to_string(),
                    },
                })
            })
            .collect::<Vec<RqliteResult>>();

        Ok(results)
    })
    .await;

    let (results, elapsed) = match res {
        Ok(res) => res,
        Err(e) => match e {
            ChangeError::TooManyRowsImpacted => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(RqliteResponse {
                        results: vec![RqliteResult::Error {
                            error: format!("too many changed columns, please restrict the number of statements per request to {}", agent.config().max_change_size),
                        }],
                        time: None,
                    }),
                );
            }
            e => {
                error!("could not execute statement(s): {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(RqliteResponse {
                        results: vec![RqliteResult::Error {
                            error: e.to_string(),
                        }],
                        time: None,
                    }),
                );
            }
        },
    };

    (
        StatusCode::OK,
        axum::Json(RqliteResponse {
            results,
            time: Some(elapsed.as_secs_f64()),
        }),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("pool connection acquisition error")]
    Pool(#[from] bb8::RunError<bb8_rusqlite::Error>),
    #[error("sqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),
}

async fn query_statements(
    pool: &SqlitePool,
    statements: &[Statement],
    associative: bool,
) -> Result<Vec<RqliteResult>, QueryError> {
    let conn = pool.get().await?;

    let mut results = vec![];

    block_in_place(|| {
        for stmt in statements.iter() {
            let start = Instant::now();
            let prepped_res = match stmt {
                Statement::Simple(q) => conn.prepare(q.as_str()),
                Statement::WithParams(params) => match params.first().and_then(|v| v.as_str()) {
                    Some(q) => conn.prepare(q),
                    None => {
                        let builder = QueryResultBuilder::new(
                            vec![],
                            vec![],
                            Some(start.elapsed().as_secs_f64()),
                        );
                        results.push(if associative {
                            builder.build_associative()
                        } else {
                            builder.build_associative()
                        });
                        continue;
                    }
                },
                Statement::WithNamedParams(q, _) => conn.prepare(q.as_str()),
            };

            let mut prepped = match prepped_res {
                Ok(prepped) => prepped,
                Err(e) => {
                    results.push(RqliteResult::Error {
                        error: e.to_string(),
                    });
                    continue;
                }
            };

            let col_names: Vec<CompactString> = prepped
                .column_names()
                .into_iter()
                .map(|s| s.to_compact_string())
                .collect();

            let col_types: Vec<Option<CompactString>> = prepped
                .columns()
                .into_iter()
                .map(|c| c.decl_type().map(|t| t.to_compact_string()))
                .collect();

            let rows_res = match stmt {
                Statement::Simple(_) => prepped.query(()),
                Statement::WithParams(params) => {
                    let mut iter = params.iter();
                    // skip 1
                    iter.next();
                    prepped.query(params_from_iter(iter))
                }
                Statement::WithNamedParams(_, params) => prepped.query(
                    params
                        .iter()
                        .map(|(k, v)| (k.as_str(), v as &dyn ToSql))
                        .collect::<Vec<(&str, &dyn ToSql)>>()
                        .as_slice(),
                ),
            };
            let elapsed = start.elapsed();

            let sqlite_rows = match rows_res {
                Ok(rows) => rows,
                Err(e) => {
                    results.push(RqliteResult::Error {
                        error: e.to_string(),
                    });
                    continue;
                }
            };

            results.push(rows_to_rqlite(
                sqlite_rows,
                col_names,
                col_types,
                elapsed,
                associative,
            ));
        }
    });

    Ok(results)
}

fn rows_to_rqlite<'stmt>(
    mut sqlite_rows: rusqlite::Rows<'stmt>,
    col_names: Vec<CompactString>,
    col_types: Vec<Option<CompactString>>,
    elapsed: Duration,
    associative: bool,
) -> RqliteResult {
    let mut builder = QueryResultBuilder::new(col_names, col_types, Some(elapsed.as_secs_f64()));

    loop {
        match sqlite_rows.next() {
            Ok(Some(row)) => {
                if let Err(e) = builder.add_row(row) {
                    return RqliteResult::Error {
                        error: e.to_string(),
                    };
                }
            }
            Ok(None) => {
                break;
            }
            Err(e) => {
                return RqliteResult::Error {
                    error: e.to_string(),
                };
            }
        }
    }

    if associative {
        builder.build_associative()
    } else {
        builder.build()
    }
}

pub async fn api_v1_db_query(
    // axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    Extension(agent): Extension<Agent>,
    axum::extract::Json(statements): axum::extract::Json<Vec<Statement>>,
) -> (StatusCode, axum::Json<RqliteResponse>) {
    if statements.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: "at least 1 statement is required".into(),
                }],
                time: None,
            }),
        );
    }

    let start = Instant::now();
    match query_statements(agent.read_only_pool(), &statements, false).await {
        Ok(results) => {
            let elapsed = start.elapsed();
            (
                StatusCode::OK,
                axum::Json(RqliteResponse {
                    results,
                    time: Some(elapsed.as_secs_f64()),
                }),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: e.to_string(),
                }],
                time: None,
            }),
        ),
    }
}

async fn execute_schema(agent: &Agent, statements: Vec<Statement>) -> eyre::Result<()> {
    let new_sql: String = statements
        .into_iter()
        .map(|stmt| match stmt {
            Statement::Simple(s) => Ok(s),
            _ => eyre::bail!("only simple statements are supported"),
        })
        .collect::<Result<Vec<_>, eyre::Report>>()?
        .join(";");

    let partial_schema = parse_sql(&new_sql)?;

    let mut conn = agent.read_write_pool().get().await?;

    // hold onto this lock so nothing else makes changes
    let mut schema_write = agent.0.schema.write();

    let mut new_schema = schema_write.clone();

    for (name, def) in partial_schema.tables.iter() {
        new_schema.tables.insert(name.clone(), def.clone());
    }

    block_in_place(|| {
        let tx = conn.transaction()?;

        make_schema_inner(&tx, &schema_write, &new_schema)?;

        for tbl_name in partial_schema.tables.keys() {
            tx.execute("DELETE FROM __corro_schema WHERE tbl_name = ?", [tbl_name])?;
            let n = tx.execute("INSERT INTO __corro_schema SELECT tbl_name, type, name, sql, 'api' AS source FROM sqlite_schema WHERE tbl_name = ? AND type IN ('table', 'index') AND name IS NOT NULL", [tbl_name])?;
            info!("updated {n} rows in __corro_schema for table {tbl_name}");
        }

        tx.commit()?;

        Ok::<_, eyre::Report>(())
    })?;

    *schema_write = new_schema;

    Ok(())
}

pub async fn api_v1_db_schema(
    Extension(agent): Extension<Agent>,
    axum::extract::Json(statements): axum::extract::Json<Vec<Statement>>,
) -> (StatusCode, axum::Json<RqliteResponse>) {
    if statements.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: "at least 1 statement is required".into(),
                }],
                time: None,
            }),
        );
    }

    if let Err(e) = execute_schema(&agent, statements).await {
        error!("could not merge schemas: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(RqliteResponse {
                results: vec![RqliteResult::Error {
                    error: e.to_string(),
                }],
                time: None,
            }),
        );
    }

    (
        StatusCode::OK,
        axum::Json(RqliteResponse {
            results: vec![],
            time: None,
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use corro_types::{
        actor::ActorId,
        config::Config,
        schema::{apply_schema, NormalizedSchema},
        sqlite::CrConnManager,
    };
    use tokio::sync::mpsc::{channel, error::TryRecvError};
    use uuid::Uuid;

    use super::*;

    use crate::agent::migrate;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn rqlite_db_execute() -> eyre::Result<()> {
        _ = tracing_subscriber::fmt::try_init();
        let dir = tempfile::tempdir()?;
        let schema_path = dir.path().join("schema");
        tokio::fs::create_dir_all(&schema_path).await?;

        tokio::fs::write(schema_path.join("test.sql"), corro_tests::TEST_SCHEMA).await?;

        let rw_pool = bb8::Pool::builder()
            .max_size(1)
            .build_unchecked(CrConnManager::new(dir.path().join("./test.sqlite")));

        {
            let mut conn = rw_pool.get().await?;
            migrate(&mut conn)?;
            apply_schema(&mut conn, &[&schema_path], &NormalizedSchema::default())?;
        }

        let (tx, mut rx) = channel(1);

        let agent = Agent(Arc::new(corro_types::agent::AgentInner {
            actor_id: ActorId(Uuid::new_v4()),
            ro_pool: bb8::Pool::builder()
                .max_size(1)
                .build_unchecked(CrConnManager::new(dir.path().join("./test.sqlite"))),
            rw_pool,
            config: ArcSwap::from_pointee(
                Config::builder()
                    .db_path(dir.path().join("corrosion.db").display().to_string())
                    .add_schema_path(schema_path.display().to_string())
                    .gossip_addr("127.0.0.1:1234".parse()?)
                    .api_addr("127.0.0.1:8080".parse()?)
                    .build()?,
            ),
            gossip_addr: "127.0.0.1:0".parse().unwrap(),
            api_addr: "127.0.0.1:0".parse().unwrap(),
            members: Default::default(),
            clock: Default::default(),
            bookie: Default::default(),
            subscribers: Default::default(),
            tx_bcast: tx,
            schema: Default::default(),
        }));

        let (status_code, body) = api_v1_db_execute(
            Extension(agent.clone()),
            axum::Json(vec![Statement::WithParams(vec![
                "insert into tests (id, text) values (?,?)".into(),
                "service-id".into(),
                "service-name".into(),
            ])]),
        )
        .await;

        println!("{body:?}");

        assert_eq!(status_code, StatusCode::OK);

        assert!(body.0.results.len() == 1);

        let msg = rx.recv().await.expect("not msg received on bcast channel");

        assert!(matches!(
            msg,
            BroadcastInput::AddBroadcast(Message::V1(MessageV1::Change { version: 1, .. }))
        ));

        assert_eq!(agent.bookie().last(&agent.actor_id()), Some(1));

        println!("second req...");

        let (status_code, body) = api_v1_db_execute(
            Extension(agent.clone()),
            axum::Json(vec![Statement::WithParams(vec![
                "update tests SET text = ? where id = ?".into(),
                "service-name".into(),
                "service-id".into(),
            ])]),
        )
        .await;

        println!("{body:?}");

        assert_eq!(status_code, StatusCode::OK);

        assert!(body.0.results.len() == 1);

        // no actual changes!
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        Ok(())
    }
}
