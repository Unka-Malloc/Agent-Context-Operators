use super::*;
use rusqlite::{types::ValueRef, Connection};
use serde_json::{json, Value};

fn spec() -> SqliteUsageSpec {
    let tokens = TokenColumns {
        input: "prompt".to_owned(),
        output: "completion".to_owned(),
        cache_read: Some("read_cache".to_owned()),
        cache_write: Some("write_cache".to_owned()),
    };
    SqliteUsageSpec {
        root: "synthetic-usage-store".to_owned(),
        file_names: vec!["usage.sqlite".to_owned()],
        sessions: SessionTable {
            table: "runs".to_owned(),
            id: "id".to_owned(),
            model: "model".to_owned(),
            started_at: "started".to_owned(),
            ended_at: "ended".to_owned(),
            requests: None,
            tokens: tokens.clone(),
        },
        details: DetailTable {
            table: "parts".to_owned(),
            session: "run".to_owned(),
            model: "model".to_owned(),
            task: "task".to_owned(),
            first_seen: "first_seen".to_owned(),
            last_seen: "last_seen".to_owned(),
            requests: None,
            tokens,
        },
        activity: ActivityTable {
            table: "activity".to_owned(),
            session: "run".to_owned(),
            timestamp: "ts".to_owned(),
        },
        input_accounting: InputAccounting::ExcludesCache,
        coverage: TaskCoverage {
            included: vec![String::new(), "included-route".to_owned()],
            additional: vec!["auxiliary-task".to_owned()],
        },
        unknown_models: vec!["not-reported-model".to_owned()],
    }
}

fn database(indexed: bool) -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE runs(id TEXT PRIMARY KEY, model TEXT, started REAL, ended REAL,
              prompt INTEGER, completion INTEGER, read_cache INTEGER, write_cache INTEGER);
             CREATE TABLE parts(counter_id INTEGER PRIMARY KEY, run TEXT, model TEXT, task TEXT, first_seen REAL, last_seen REAL,
              prompt INTEGER, completion INTEGER, read_cache INTEGER, write_cache INTEGER);
             CREATE TABLE activity(run TEXT, ts REAL);",
        )
        .unwrap();
    if indexed {
        connection
            .execute_batch("CREATE INDEX activity_by_run ON activity(run, ts);")
            .unwrap();
    }
    connection
}

fn scalar(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => json!(std::str::from_utf8(value).unwrap()),
        ValueRef::Blob(_) => panic!("synthetic metadata must not contain blobs"),
    }
}

fn capture(connection: &Connection, spec: &SqliteUsageSpec) -> SchemaCapture {
    let plan = schema_plan(spec).unwrap();
    assert_eq!(plan.queries.len(), 4);
    let mut result = SchemaCapture::new();
    for query in plan.queries {
        let mut statement = connection.prepare(&query.sql).unwrap();
        assert!(statement.readonly());
        assert_eq!(query.output, QueryOutput::Rows);
        let names = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let rows = statement
            .query_map([], |row| {
                names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| Ok((name.clone(), scalar(row.get_ref(index)?))))
                    .collect::<rusqlite::Result<_>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        result.insert(query.id, rows);
    }
    result
}

fn records(connection: &Connection, spec: &SqliteUsageSpec) -> Vec<Value> {
    let plan = data_plan(spec, &capture(connection, spec)).unwrap();
    assert_eq!(plan.queries.len(), 1);
    assert_eq!(plan.queries[0].output, QueryOutput::JsonRecords);
    let mut statement = connection.prepare(&plan.queries[0].sql).unwrap();
    assert!(statement.readonly());
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
        .collect()
}

fn populate(connection: &Connection) {
    connection
        .execute_batch(
            "INSERT INTO runs VALUES('synthetic-run', 'selected-model', 100, NULL, 100, 10, 0, 0);
         INSERT INTO parts(run,model,task,first_seen,last_seen,prompt,completion,read_cache,write_cache) VALUES('synthetic-run','aux-model','auxiliary-task',110,130,20,5,0,0);
         INSERT INTO activity VALUES('synthetic-run',150);",
        )
        .unwrap();
}

#[test]
fn indexed_and_unindexed_activity_produce_the_same_read_only_records() {
    let spec = spec();
    let indexed = database(true);
    let unindexed = database(false);
    populate(&indexed);
    populate(&unindexed);
    let indexed_records = records(&indexed, &spec);
    let unindexed_records = records(&unindexed, &spec);
    assert_eq!(indexed_records, unindexed_records);
    let result = decode_record(&spec, &indexed_records[0]).unwrap();
    assert_eq!(result.tokens.input, Some(100));
    assert_eq!(result.details[0].tokens.input, Some(20));
    assert_eq!(result.details[0].coverage, Coverage::Additional);
    assert_eq!(result.last_activity_at, 150.0);
}

#[test]
fn valid_empty_store_and_unsupported_schema_are_distinct() {
    let spec = spec();
    assert!(records(&database(false), &spec).is_empty());
    let unsupported = Connection::open_in_memory().unwrap();
    assert_eq!(
        data_plan(&spec, &capture(&unsupported, &spec)),
        Err(UsageReadFailure::UnsupportedSchema)
    );
}

#[test]
fn orphan_details_are_reported_instead_of_disappearing_in_a_join() {
    let spec = spec();
    let connection = database(false);
    connection.execute_batch(
        "INSERT INTO parts(run,model,task,first_seen,last_seen,prompt,completion,read_cache,write_cache) VALUES('missing-run','aux-model','auxiliary-task',110,130,20,5,0,0);",
    ).unwrap();
    let rows = records(&connection, &spec);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        decode_record(&spec, &rows[0]),
        Err(UsageReadFailure::OrphanDetail)
    );
}

#[test]
fn missing_metrics_and_unknown_task_relations_remain_unknown() {
    let spec = spec();
    let connection = database(false);
    populate(&connection);
    connection
        .execute_batch(
            "UPDATE runs SET prompt=NULL;
         UPDATE parts SET task='not-declared';",
        )
        .unwrap();
    let result = decode_record(&spec, &records(&connection, &spec)[0]).unwrap();
    assert_eq!(result.tokens.input, None);
    assert_eq!(result.details[0].coverage, Coverage::Unknown);
}

#[test]
fn invalid_numeric_storage_is_not_a_zero() {
    let spec = spec();
    for literal in ["-1", "0.5", "'invalid-number'"] {
        let connection = database(false);
        populate(&connection);
        connection
            .execute_batch(&format!("UPDATE runs SET prompt={literal};"))
            .unwrap();
        assert_eq!(
            decode_record(&spec, &records(&connection, &spec)[0]),
            Err(UsageReadFailure::InvalidRecord)
        );
    }
}

#[test]
fn large_integer_readings_do_not_pass_through_a_float() {
    let spec = spec();
    let connection = database(false);
    populate(&connection);
    connection
        .execute_batch("UPDATE runs SET prompt=9007199254740993;")
        .unwrap();
    let result = decode_record(&spec, &records(&connection, &spec)[0]).unwrap();
    assert_eq!(result.tokens.input, Some(9_007_199_254_740_993));
}

#[test]
fn coverage_declarations_cannot_place_a_task_in_both_sets() {
    let mut spec = spec();
    spec.coverage.additional.push("included-route".to_owned());
    assert_eq!(
        schema_plan(&spec),
        Err(UsageReadFailure::InvalidDeclaration)
    );
}

#[test]
fn fallback_activity_does_not_depend_on_detail_order() {
    let spec = spec();
    let connection = database(false);
    populate(&connection);
    connection.execute_batch(
        "INSERT INTO parts(run,model,task,first_seen,last_seen,prompt,completion,read_cache,write_cache) VALUES('synthetic-run','late-model','auxiliary-task',110,200,5,1,0,0);
         INSERT INTO parts(run,model,task,first_seen,last_seen,prompt,completion,read_cache,write_cache) VALUES('synthetic-run','unknown-time-model','auxiliary-task',110,NULL,5,1,0,0);",
    ).unwrap();
    let mut record = records(&connection, &spec).remove(0);
    let first = decode_record(&spec, &record).unwrap();
    record["details"].as_array_mut().unwrap().reverse();
    let second = decode_record(&spec, &record).unwrap();
    let fallback = |session: &UsageSession| {
        session
            .details
            .iter()
            .find(|detail| detail.model.as_deref() == Some("unknown-time-model"))
            .unwrap()
            .last_seen
    };
    assert_eq!(fallback(&first), 150.0);
    assert_eq!(fallback(&second), 150.0);
    assert_eq!(first.last_activity_at, second.last_activity_at);
    assert!(
        first
            .details
            .iter()
            .find(|detail| detail.model.as_deref() == Some("unknown-time-model"))
            .unwrap()
            .time_inferred
    );
}

#[test]
fn model_absence_uses_the_source_declaration_not_a_global_name_guess() {
    let spec = spec();
    let connection = database(false);
    populate(&connection);
    connection
        .execute_batch(
            "UPDATE runs SET model='not-reported-model';
             UPDATE parts SET model='unknown';",
        )
        .unwrap();
    let result = decode_record(&spec, &records(&connection, &spec)[0]).unwrap();
    assert_eq!(result.model, None);
    assert_eq!(result.details[0].model.as_deref(), Some("unknown"));
}

#[test]
fn detail_counters_need_a_stable_declared_database_key() {
    let spec = spec();
    let mut schema = capture(&database(false), &spec);
    for column in schema.get_mut("details").unwrap() {
        column.insert("pk".to_owned(), json!(0));
    }
    assert_eq!(
        data_plan(&spec, &schema),
        Err(UsageReadFailure::UnsupportedSchema)
    );
}

#[test]
fn decoded_counter_keys_are_stable_opaque_and_unique() {
    let spec = spec();
    let connection = database(false);
    populate(&connection);
    let mut record = records(&connection, &spec).remove(0);
    record["details"][0]["identity"] = json!(["synthetic-private-billing-route"]);
    let first = decode_record(&spec, &record).unwrap();
    let repeated = decode_record(&spec, &record).unwrap();
    assert_eq!(first.details[0].key, repeated.details[0].key);
    assert!(first.details[0].key.starts_with("detail:"));
    assert!(!serde_json::to_string(&first)
        .unwrap()
        .contains("synthetic-private-billing-route"));
    record["details"][0]["identity"] = json!(["another-synthetic-route"]);
    assert_ne!(
        first.details[0].key,
        decode_record(&spec, &record).unwrap().details[0].key
    );
    let duplicate = record["details"][0].clone();
    record["details"].as_array_mut().unwrap().push(duplicate);
    assert_eq!(
        decode_record(&spec, &record),
        Err(UsageReadFailure::InvalidRecord)
    );
}

#[test]
fn request_counts_are_declared_values_not_row_counts() {
    let mut spec = spec();
    spec.sessions.requests = Some("calls".to_owned());
    spec.details.requests = Some("calls".to_owned());
    let connection = database(false);
    populate(&connection);
    connection
        .execute_batch(
            "ALTER TABLE runs ADD COLUMN calls INTEGER;
         ALTER TABLE parts ADD COLUMN calls INTEGER;
         UPDATE runs SET calls=4;
         UPDATE parts SET calls=1;",
        )
        .unwrap();
    let decoded = decode_record(&spec, &records(&connection, &spec)[0]).unwrap();
    assert_eq!(decoded.requests, Some(4));
    assert_eq!(decoded.details[0].requests, Some(1));
    connection
        .execute("UPDATE runs SET calls=NULL", [])
        .unwrap();
    assert_eq!(
        decode_record(&spec, &records(&connection, &spec)[0])
            .unwrap()
            .requests,
        None
    );
    connection.execute("UPDATE runs SET calls=-1", []).unwrap();
    assert_eq!(
        decode_record(&spec, &records(&connection, &spec)[0]),
        Err(UsageReadFailure::InvalidRecord)
    );
}

#[test]
fn a_request_counter_needs_matching_coverage_on_both_tables() {
    let mut spec = spec();
    spec.sessions.requests = Some("calls".to_owned());
    assert_eq!(
        schema_plan(&spec),
        Err(UsageReadFailure::InvalidDeclaration)
    );
}
