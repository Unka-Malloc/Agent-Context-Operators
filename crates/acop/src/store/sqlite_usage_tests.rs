use super::*;
use acop_usage_core::ReadQuery;
use serde_json::json;

fn database() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic.sqlite");
    let connection = Connection::open(&path).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .execute_batch("CREATE TABLE readings(value INTEGER); INSERT INTO readings VALUES(1);")
        .unwrap();
    (directory, path, connection)
}

fn plan(sql: &str, output: QueryOutput) -> ReadPlan {
    ReadPlan {
        queries: vec![ReadQuery {
            id: "synthetic-query".to_owned(),
            sql: sql.to_owned(),
            output,
        }],
    }
}

#[test]
fn a_snapshot_keeps_one_database_view_across_queries() {
    let (_directory, path, writer) = database();
    let query = plan("SELECT value FROM readings", QueryOutput::Rows);
    let snapshot = SqliteUsageSnapshot::open(&path).unwrap();
    let initial = snapshot.capture_schema(&query).unwrap();
    assert_eq!(initial["synthetic-query"][0]["value"], json!(1));
    writer.execute("UPDATE readings SET value=2", []).unwrap();
    assert_eq!(snapshot.capture_schema(&query).unwrap(), initial);
    drop(snapshot);
    let next = SqliteUsageSnapshot::open(&path).unwrap();
    assert_eq!(
        next.capture_schema(&query).unwrap()["synthetic-query"][0]["value"],
        json!(2)
    );
}

#[test]
fn acquisition_cannot_write_to_the_source() {
    let (_directory, path, writer) = database();
    let snapshot = SqliteUsageSnapshot::open(&path).unwrap();
    assert_eq!(
        snapshot.capture_schema(&plan("UPDATE readings SET value=9", QueryOutput::Rows)),
        Err(SqliteUsageReadFailure::InvalidPlan)
    );
    assert_eq!(
        writer
            .query_row("SELECT value FROM readings", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn invalid_cells_and_plans_are_refused_instead_of_becoming_zero() {
    let (_directory, path, _writer) = database();
    let snapshot = SqliteUsageSnapshot::open(&path).unwrap();
    for sql in [
        "SELECT X'01' AS value",
        "SELECT CAST(X'ff' AS TEXT) AS value",
        "SELECT 1e999 AS value",
    ] {
        assert_eq!(
            snapshot.capture_schema(&plan(sql, QueryOutput::Rows)),
            Err(SqliteUsageReadFailure::InvalidValue)
        );
    }
    let mut duplicate = plan("SELECT 1 AS value", QueryOutput::Rows);
    duplicate.queries.push(duplicate.queries[0].clone());
    for query in [
        duplicate,
        ReadPlan {
            queries: Vec::new(),
        },
        plan("SELECT 1", QueryOutput::JsonRecords),
    ] {
        assert_eq!(
            snapshot.capture_schema(&query),
            Err(SqliteUsageReadFailure::InvalidPlan)
        );
    }
}

#[test]
fn records_stream_and_consumer_failures_are_not_hidden() {
    let (_directory, path, _writer) = database();
    let snapshot = SqliteUsageSnapshot::open(&path).unwrap();
    let query = plan("SELECT '{\"counter\":1}'", QueryOutput::JsonRecords);
    let mut records = Vec::new();
    snapshot
        .visit_records(&query, |record| {
            records.push(record);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(records, vec![json!({"counter":1})]);
    assert_eq!(
        snapshot.visit_records(&query, |_| Err("synthetic-consumer-failure")),
        Err(SqliteUsageVisitFailure::Consumer(
            "synthetic-consumer-failure"
        ))
    );
    assert_eq!(
        snapshot.visit_records(
            &plan("SELECT 'not-json'", QueryOutput::JsonRecords),
            |_| Ok::<_, ()>(())
        ),
        Err(SqliteUsageVisitFailure::Read(
            SqliteUsageReadFailure::InvalidValue
        ))
    );
    assert_eq!(
        snapshot.visit_records(&plan("SELECT 1, 2", QueryOutput::JsonRecords), |_| Ok::<
            _,
            (),
        >(
            ()
        )),
        Err(SqliteUsageVisitFailure::Read(
            SqliteUsageReadFailure::InvalidPlan
        ))
    );
    let mut visits = 0;
    snapshot
        .visit_records(
            &plan("SELECT '{\"counter\":1}' WHERE 0", QueryOutput::JsonRecords),
            |_| {
                visits += 1;
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert_eq!(visits, 0);
}

#[test]
fn missing_databases_are_not_created_and_errors_do_not_echo_paths() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-private-sentinel.sqlite");
    let error = match SqliteUsageSnapshot::open(&path) {
        Ok(_) => panic!("missing store must fail"),
        Err(error) => error,
    };
    assert_eq!(error, SqliteUsageReadFailure::Open);
    assert_eq!(error.to_string(), "usage_store_open_failed");
    assert!(!path.exists());
}
