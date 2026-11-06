use acop_usage_core::{QueryOutput, ReadPlan, ReadQuery, SchemaCapture};
use rusqlite::{types::ValueRef, Connection, OpenFlags, Statement};
use serde_json::{Number, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqliteUsageReadFailure {
    Open,
    Query,
    InvalidPlan,
    InvalidValue,
}
impl std::fmt::Display for SqliteUsageReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Open => "usage_store_open_failed",
            Self::Query => "usage_store_query_failed",
            Self::InvalidPlan => "usage_store_plan_invalid",
            Self::InvalidValue => "usage_store_value_invalid",
        })
    }
}
impl std::error::Error for SqliteUsageReadFailure {}
impl From<rusqlite::Error> for SqliteUsageReadFailure {
    fn from(_: rusqlite::Error) -> Self {
        Self::Query
    }
}
#[derive(Debug, Eq, PartialEq)]
pub enum SqliteUsageVisitFailure<E> {
    Read(SqliteUsageReadFailure),
    Consumer(E),
}
impl<E> From<SqliteUsageReadFailure> for SqliteUsageVisitFailure<E> {
    fn from(value: SqliteUsageReadFailure) -> Self {
        Self::Read(value)
    }
}
impl<E> From<rusqlite::Error> for SqliteUsageVisitFailure<E> {
    fn from(_: rusqlite::Error) -> Self {
        Self::Read(SqliteUsageReadFailure::Query)
    }
}

pub struct SqliteUsageSnapshot {
    connection: Connection,
}
impl SqliteUsageSnapshot {
    pub fn open(path: &Path) -> Result<Self, SqliteUsageReadFailure> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| SqliteUsageReadFailure::Open)?;
        connection.execute_batch("PRAGMA query_only = ON; BEGIN DEFERRED;")?;
        Ok(Self { connection })
    }
    fn statement(&self, query: &ReadQuery) -> Result<Statement<'_>, SqliteUsageReadFailure> {
        let statement = self.connection.prepare(&query.sql)?;
        if !statement.readonly() || statement.column_count() == 0 {
            return Err(SqliteUsageReadFailure::InvalidPlan);
        }
        Ok(statement)
    }
    pub fn capture_schema(&self, plan: &ReadPlan) -> Result<SchemaCapture, SqliteUsageReadFailure> {
        validate_plan(plan, QueryOutput::Rows)?;
        let mut capture = SchemaCapture::new();
        for query in &plan.queries {
            let mut statement = self.statement(query)?;
            let names = statement
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if names.iter().collect::<BTreeSet<_>>().len() != names.len() {
                return Err(SqliteUsageReadFailure::InvalidPlan);
            }
            let mut rows = statement.query([])?;
            let mut values = Vec::new();
            while let Some(row) = rows.next()? {
                let mut value = BTreeMap::new();
                for (index, name) in names.iter().enumerate() {
                    value.insert(name.clone(), scalar(row.get_ref(index)?)?);
                }
                values.push(value);
            }
            capture.insert(query.id.clone(), values);
        }
        Ok(capture)
    }
    pub fn visit_records<E>(
        &self,
        plan: &ReadPlan,
        mut visit: impl FnMut(Value) -> Result<(), E>,
    ) -> Result<(), SqliteUsageVisitFailure<E>> {
        validate_plan(plan, QueryOutput::JsonRecords)?;
        for query in &plan.queries {
            let mut statement = self.statement(query)?;
            if statement.column_count() != 1 {
                return Err(SqliteUsageReadFailure::InvalidPlan.into());
            }
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let ValueRef::Text(text) = row.get_ref(0)? else {
                    return Err(SqliteUsageReadFailure::InvalidValue.into());
                };
                let value = serde_json::from_slice(text)
                    .map_err(|_| SqliteUsageReadFailure::InvalidValue)?;
                visit(value).map_err(SqliteUsageVisitFailure::Consumer)?;
            }
        }
        Ok(())
    }
}
fn validate_plan(plan: &ReadPlan, output: QueryOutput) -> Result<(), SqliteUsageReadFailure> {
    let mut ids = BTreeSet::new();
    if plan.queries.is_empty()
        || plan.queries.iter().any(|query| {
            query.output != output || query.id.is_empty() || !ids.insert(query.id.as_str())
        })
    {
        return Err(SqliteUsageReadFailure::InvalidPlan);
    }
    Ok(())
}
fn scalar(value: ValueRef<'_>) -> Result<Value, SqliteUsageReadFailure> {
    match value {
        ValueRef::Null => Ok(Value::Null),
        ValueRef::Integer(value) => Ok(Value::from(value)),
        ValueRef::Real(value) => Number::from_f64(value)
            .map(Value::Number)
            .ok_or(SqliteUsageReadFailure::InvalidValue),
        ValueRef::Text(value) => std::str::from_utf8(value)
            .map(|value| Value::String(value.to_owned()))
            .map_err(|_| SqliteUsageReadFailure::InvalidValue),
        ValueRef::Blob(_) => Err(SqliteUsageReadFailure::InvalidValue),
    }
}
#[cfg(test)]
#[path = "sqlite_usage_tests.rs"]
mod tests;
