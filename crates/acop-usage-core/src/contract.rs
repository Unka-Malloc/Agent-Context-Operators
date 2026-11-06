use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenColumns {
    pub input: String,
    pub output: String,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionTable {
    pub table: String,
    pub id: String,
    pub model: String,
    pub started_at: String,
    pub ended_at: String,
    pub requests: Option<String>,
    pub tokens: TokenColumns,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DetailTable {
    pub table: String,
    pub session: String,
    pub model: String,
    pub task: String,
    pub first_seen: String,
    pub last_seen: String,
    pub requests: Option<String>,
    pub tokens: TokenColumns,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityTable {
    pub table: String,
    pub session: String,
    pub timestamp: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputAccounting {
    ExcludesCache,
    IncludesCache,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskCoverage {
    pub included: Vec<String>,
    pub additional: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SqliteUsageSpec {
    pub root: String,
    pub file_names: Vec<String>,
    pub sessions: SessionTable,
    pub details: DetailTable,
    pub activity: ActivityTable,
    pub input_accounting: InputAccounting,
    pub coverage: TaskCoverage,
    pub unknown_models: Vec<String>,
}

impl SqliteUsageSpec {
    pub fn from_json(input: &str) -> Result<Self, UsageReadFailure> {
        serde_json::from_str(input).map_err(|_| UsageReadFailure::InvalidDeclaration)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueryOutput {
    Rows,
    JsonRecords,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadQuery {
    pub id: String,
    pub sql: String,
    pub output: QueryOutput,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadPlan {
    pub queries: Vec<ReadQuery>,
}

pub type SchemaCapture = BTreeMap<String, Vec<BTreeMap<String, Value>>>;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReportedTokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    Included,
    Additional,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageDetail {
    pub key: String,
    pub model: Option<String>,
    pub task: String,
    pub first_seen: f64,
    pub last_seen: f64,
    pub time_inferred: bool,
    pub tokens: ReportedTokens,
    pub requests: Option<u64>,
    pub coverage: Coverage,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageSession {
    pub id: String,
    pub model: Option<String>,
    pub started_at: f64,
    pub last_activity_at: f64,
    pub tokens: ReportedTokens,
    pub requests: Option<u64>,
    pub details: Vec<UsageDetail>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageReadFailure {
    InvalidDeclaration,
    UnsupportedSchema,
    InvalidRecord,
    OrphanDetail,
}

impl std::fmt::Display for UsageReadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidDeclaration => "usage_declaration_invalid",
            Self::UnsupportedSchema => "usage_schema_unsupported",
            Self::InvalidRecord => "usage_record_invalid",
            Self::OrphanDetail => "usage_detail_session_missing",
        })
    }
}

impl std::error::Error for UsageReadFailure {}
