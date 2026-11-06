use crate::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

type Result<T> = std::result::Result<T, UsageReadFailure>;

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn field(alias: &str, name: &str) -> String {
    format!("{alias}.{}", identifier(name))
}

fn optional_field(alias: &str, name: &Option<String>) -> String {
    name.as_ref()
        .map(|name| field(alias, name))
        .unwrap_or_else(|| "NULL".to_owned())
}

fn counter_columns(tokens: &TokenColumns) -> Vec<&str> {
    let mut fields = vec![tokens.input.as_str(), tokens.output.as_str()];
    fields.extend(tokens.cache_read.as_deref());
    fields.extend(tokens.cache_write.as_deref());
    fields
}

fn requirements(spec: &SqliteUsageSpec) -> Vec<(&str, &str, Vec<&str>)> {
    let sessions = &spec.sessions;
    let details = &spec.details;
    let activity = &spec.activity;
    let mut session_fields = vec![
        sessions.id.as_str(),
        sessions.model.as_str(),
        sessions.started_at.as_str(),
        sessions.ended_at.as_str(),
    ];
    session_fields.extend(counter_columns(&sessions.tokens));
    session_fields.extend(sessions.requests.as_deref());
    let mut detail_fields = vec![
        details.session.as_str(),
        details.model.as_str(),
        details.task.as_str(),
        details.first_seen.as_str(),
        details.last_seen.as_str(),
    ];
    detail_fields.extend(counter_columns(&details.tokens));
    detail_fields.extend(details.requests.as_deref());
    vec![
        ("sessions", &sessions.table, session_fields),
        ("details", &details.table, detail_fields),
        (
            "activity",
            &activity.table,
            vec![&activity.session, &activity.timestamp],
        ),
    ]
}

fn validate(spec: &SqliteUsageSpec) -> Result<()> {
    if spec.root.is_empty()
        || spec.sessions.requests.is_some() != spec.details.requests.is_some()
        || spec.file_names.is_empty()
        || spec.file_names.iter().any(|name| {
            name.is_empty() || name.contains(['\0', '/', '\\']) || name == "." || name == ".."
        })
        || requirements(spec).iter().any(|(_, table, columns)| {
            table.is_empty()
                || table.contains('\0')
                || columns
                    .iter()
                    .any(|column| column.is_empty() || column.contains('\0'))
        })
    {
        return Err(UsageReadFailure::InvalidDeclaration);
    }
    let names = spec
        .coverage
        .included
        .iter()
        .chain(&spec.coverage.additional)
        .collect::<Vec<_>>();
    if names.iter().copied().collect::<BTreeSet<_>>().len() != names.len() {
        return Err(UsageReadFailure::InvalidDeclaration);
    }
    for columns in [&spec.sessions.tokens, &spec.details.tokens] {
        if columns.input == columns.output
            || columns
                .cache_read
                .as_ref()
                .is_some_and(|read| columns.cache_write.as_ref() == Some(read))
            || (spec.input_accounting == InputAccounting::ExcludesCache
                && counter_columns(columns)
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != counter_columns(columns).len())
        {
            return Err(UsageReadFailure::InvalidDeclaration);
        }
    }
    Ok(())
}

pub fn schema_plan(spec: &SqliteUsageSpec) -> Result<ReadPlan> {
    validate(spec)?;
    let mut queries = requirements(spec)
        .into_iter()
        .map(|(id, table, _)| ReadQuery {
            id: id.to_owned(),
            sql: format!("PRAGMA table_info({})", identifier(table)),
            output: QueryOutput::Rows,
        })
        .collect::<Vec<_>>();
    queries.push(ReadQuery {
        id: "activity-indexes".to_owned(),
        sql: format!(
            "SELECT il.partial, ii.seqno, ii.name \
             FROM pragma_index_list({}) AS il \
             JOIN pragma_index_info(il.name) AS ii",
            literal(&spec.activity.table)
        ),
        output: QueryOutput::Rows,
    });
    Ok(ReadPlan { queries })
}

fn token_object(alias: &str, tokens: &TokenColumns) -> String {
    format!(
        "'input', {}, 'output', {}, 'cacheRead', {}, 'cacheWrite', {}",
        field(alias, &tokens.input),
        field(alias, &tokens.output),
        optional_field(alias, &tokens.cache_read),
        optional_field(alias, &tokens.cache_write)
    )
}

pub fn data_plan(spec: &SqliteUsageSpec, schema: &SchemaCapture) -> Result<ReadPlan> {
    validate(spec)?;
    for (id, _, required) in requirements(spec) {
        let rows = schema.get(id).ok_or(UsageReadFailure::UnsupportedSchema)?;
        let present = rows
            .iter()
            .filter_map(|row| row.get("name").and_then(Value::as_str))
            .collect::<BTreeSet<_>>();
        if required.iter().any(|name| !present.contains(name)) {
            return Err(UsageReadFailure::UnsupportedSchema);
        }
    }
    let primary = schema["sessions"]
        .iter()
        .filter(|row| row.get("pk").and_then(Value::as_u64).unwrap_or(0) > 0)
        .collect::<Vec<_>>();
    if primary.len() != 1
        || primary[0].get("name").and_then(Value::as_str) != Some(spec.sessions.id.as_str())
    {
        return Err(UsageReadFailure::UnsupportedSchema);
    }
    let mut detail_key = schema["details"]
        .iter()
        .filter_map(|row| {
            let position = row.get("pk")?.as_u64()?;
            let name = row.get("name")?.as_str()?;
            (position > 0).then_some((position, name))
        })
        .collect::<Vec<_>>();
    detail_key.sort_unstable_by_key(|(position, _)| *position);
    if detail_key.is_empty() {
        return Err(UsageReadFailure::UnsupportedSchema);
    }
    let identity = detail_key
        .iter()
        .map(|(_, name)| field("d", name))
        .collect::<Vec<_>>()
        .join(", ");
    let activity_indexes = schema
        .get("activity-indexes")
        .ok_or(UsageReadFailure::UnsupportedSchema)?;
    let indexed_activity = activity_indexes.iter().any(|row| {
        row.get("partial").and_then(Value::as_u64) == Some(0)
            && row.get("seqno").and_then(Value::as_u64) == Some(0)
            && row.get("name").and_then(Value::as_str) == Some(spec.activity.session.as_str())
    });
    let sessions = &spec.sessions;
    let details = &spec.details;
    let activity = &spec.activity;
    let detail_object = format!(
        "json_object('identity', json_array({identity}), 'model', {}, 'task', {}, 'firstSeen', {}, 'lastSeen', {}, 'requests', {}, {})",
        field("d", &details.model),
        field("d", &details.task),
        field("d", &details.first_seen),
        field("d", &details.last_seen),
        optional_field("d", &details.requests),
        token_object("d", &details.tokens)
    );
    let mut ctes = format!(
        "usage_details AS (SELECT {} AS owner, json_group_array({detail_object}) AS entries \
         FROM {} AS d GROUP BY {})",
        field("d", &details.session),
        identifier(&details.table),
        field("d", &details.session)
    );
    let (last_message, activity_join) = if indexed_activity {
        (
            format!(
                "(SELECT MAX({}) FROM {} AS m WHERE {} = {})",
                field("m", &activity.timestamp),
                identifier(&activity.table),
                field("m", &activity.session),
                field("s", &sessions.id)
            ),
            String::new(),
        )
    } else {
        ctes.push_str(&format!(
            ", usage_activity AS (SELECT {} AS owner, MAX({}) AS latest \
             FROM {} AS m GROUP BY {})",
            field("m", &activity.session),
            field("m", &activity.timestamp),
            identifier(&activity.table),
            field("m", &activity.session)
        ));
        (
            "a.latest".to_owned(),
            format!(
                " LEFT JOIN usage_activity AS a ON a.owner = {}",
                field("s", &sessions.id)
            ),
        )
    };
    let sql = format!(
        "WITH {ctes} \
         SELECT '{{\"issue\":\"orphan-detail\"}}' AS record WHERE EXISTS \
         (SELECT 1 FROM usage_details AS u \
          LEFT JOIN {} AS s ON {} = u.owner WHERE {} IS NULL) \
         UNION ALL \
         SELECT json_object('id', {}, 'model', {}, 'startedAt', {}, 'endedAt', {}, \
         'lastMessageAt', {last_message}, 'requests', {}, {}, 'details', json(COALESCE(u.entries, '[]'))) \
         AS record FROM {} AS s LEFT JOIN usage_details AS u ON u.owner = {}{activity_join}",
        identifier(&sessions.table),
        field("s", &sessions.id),
        field("s", &sessions.id),
        field("s", &sessions.id),
        field("s", &sessions.model),
        field("s", &sessions.started_at),
        field("s", &sessions.ended_at),
        optional_field("s", &sessions.requests),
        token_object("s", &sessions.tokens),
        identifier(&sessions.table),
        field("s", &sessions.id)
    );
    Ok(ReadPlan {
        queries: vec![ReadQuery {
            id: "records".to_owned(),
            sql,
            output: QueryOutput::JsonRecords,
        }],
    })
}

fn member<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .as_object()
        .and_then(|object| object.get(key))
        .ok_or(UsageReadFailure::InvalidRecord)
}

fn optional_model(spec: &SqliteUsageSpec, value: &Value) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => {
            let value = value.trim();
            Ok(
                (!value.is_empty() && !spec.unknown_models.iter().any(|model| model == value))
                    .then(|| value.to_owned()),
            )
        }
        _ => Err(UsageReadFailure::InvalidRecord),
    }
}

fn counter(value: &Value) -> Result<Option<u64>> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_u64()
            .map(Some)
            .ok_or(UsageReadFailure::InvalidRecord)
    }
}

fn request_count(column: &Option<String>, value: &Value) -> Result<Option<u64>> {
    let value = counter(value)?;
    if column.is_none() && value.is_some() {
        return Err(UsageReadFailure::InvalidRecord);
    }
    Ok(value)
}

fn tokens(value: &Value) -> Result<ReportedTokens> {
    Ok(ReportedTokens {
        input: counter(member(value, "input")?)?,
        output: counter(member(value, "output")?)?,
        cache_read: counter(member(value, "cacheRead")?)?,
        cache_write: counter(member(value, "cacheWrite")?)?,
    })
}

fn timestamp(value: &Value) -> Result<Option<f64>> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Some)
            .ok_or(UsageReadFailure::InvalidRecord)
    }
}

pub fn decode_record(spec: &SqliteUsageSpec, record: &Value) -> Result<UsageSession> {
    validate(spec)?;
    if record.get("issue") == Some(&Value::String("orphan-detail".to_owned())) {
        return Err(UsageReadFailure::OrphanDetail);
    }
    let id = member(record, "id")?
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(UsageReadFailure::InvalidRecord)?
        .to_owned();
    let started_at =
        timestamp(member(record, "startedAt")?)?.ok_or(UsageReadFailure::InvalidRecord)?;
    let ended_at = timestamp(member(record, "endedAt")?)?;
    let last_message = timestamp(member(record, "lastMessageAt")?)?;
    let session_activity_at = last_message.or(ended_at).unwrap_or(started_at);
    let mut last_activity_at = session_activity_at;
    let mut details = Vec::new();
    let mut keys = BTreeSet::new();
    for detail in member(record, "details")?
        .as_array()
        .ok_or(UsageReadFailure::InvalidRecord)?
    {
        let identity = member(detail, "identity")?
            .as_array()
            .filter(|values| {
                !values.is_empty()
                    && values.iter().all(|value| {
                        matches!(value, Value::String(_) | Value::Number(_) | Value::Null)
                    })
            })
            .ok_or(UsageReadFailure::InvalidRecord)?;
        let material = serde_json::to_vec(&(&spec.root, &id, identity))
            .map_err(|_| UsageReadFailure::InvalidRecord)?;
        let key = format!("detail:{:x}", Sha256::digest(material));
        if !keys.insert(key.clone()) {
            return Err(UsageReadFailure::InvalidRecord);
        }
        let task = member(detail, "task")?
            .as_str()
            .ok_or(UsageReadFailure::InvalidRecord)?
            .to_owned();
        let first_reported = timestamp(member(detail, "firstSeen")?)?;
        let last_reported = timestamp(member(detail, "lastSeen")?)?;
        let first_seen = first_reported.unwrap_or(started_at);
        let last_seen = last_reported.unwrap_or(session_activity_at);
        if first_seen > last_seen {
            return Err(UsageReadFailure::InvalidRecord);
        }
        let coverage = if spec.coverage.included.contains(&task) {
            Coverage::Included
        } else if spec.coverage.additional.contains(&task) {
            Coverage::Additional
        } else {
            Coverage::Unknown
        };
        details.push(UsageDetail {
            key,
            model: optional_model(spec, member(detail, "model")?)?,
            task,
            first_seen,
            last_seen,
            time_inferred: first_reported.is_none() || last_reported.is_none(),
            tokens: tokens(detail)?,
            requests: request_count(&spec.details.requests, member(detail, "requests")?)?,
            coverage,
        });
        last_activity_at = last_activity_at.max(last_seen);
    }
    if started_at > last_activity_at {
        return Err(UsageReadFailure::InvalidRecord);
    }
    Ok(UsageSession {
        id,
        model: optional_model(spec, member(record, "model")?)?,
        started_at,
        last_activity_at,
        tokens: tokens(record)?,
        requests: request_count(&spec.sessions.requests, member(record, "requests")?)?,
        details,
    })
}
#[cfg(test)]
#[path = "kernel_tests.rs"]
mod tests;
