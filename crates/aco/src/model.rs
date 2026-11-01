//! The canonical conversation model: `aco-conversation/1`.
//!
//! `RecordKind` is the superset every codec projects from. A codec's
//! [`crate::codec::CapabilityMatrix`] declares which kinds its native
//! format carries natively, which it adapts, and which it cannot express.

use serde::{Deserialize, Serialize};

/// Document format tag written into every exported conversation.
pub const FORMAT: &str = "aco-conversation/1";

/// One agent conversation, normalized.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    /// Always [`FORMAT`]; rejected on read when different.
    pub format: String,
    /// Source agent id (`codex`, `claude`, …). Absent for hand-authored
    /// canonical documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Session identifier meaningful inside the source store.
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Working directory the session ran in, when the store records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// RFC 3339 timestamps when the store records them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub records: Vec<Record>,
}

impl Conversation {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            format: FORMAT.to_string(),
            agent: None,
            session_id: session_id.into(),
            title: None,
            cwd: None,
            created_at: None,
            updated_at: None,
            records: Vec::new(),
        }
    }

    /// Parse a canonical document; rejects foreign format tags.
    pub fn from_json(text: &str) -> Result<Self, crate::Error> {
        let doc: Conversation =
            serde_json::from_str(text).map_err(|e| crate::Error::Document(e.to_string()))?;
        if doc.format != FORMAT {
            return Err(crate::Error::Document(format!(
                "unsupported format `{}` (expected `{FORMAT}`)",
                doc.format
            )));
        }
        Ok(doc)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("conversation serializes")
    }
}

/// One normalized record. `ordinal` is the record's position in
/// `Conversation::records`; `id`/`parent` carry the store's own node
/// identity when it has one (e.g. Claude `uuid`/`parentUuid`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    pub ordinal: u64,
    pub body: Body,
}

impl Record {
    pub fn kind(&self) -> RecordKind {
        self.body.kind()
    }

    /// Text content this record contributes to search.
    pub fn text(&self) -> String {
        self.body.text()
    }
}

/// The superset record vocabulary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Body {
    /// A human-authored turn.
    User {
        text: String,
        /// Model the request was attributed to when the store records
        /// per-turn attribution (e.g. a Cursor bubble's `modelInfo`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// A model-authored turn.
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Model reasoning / thinking content — a "process quantity".
    Reasoning { text: String },
    /// A tool/function invocation issued by the agent.
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
    },
    /// A tool/function result returned to the agent.
    ToolResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: serde_json::Value,
        #[serde(default)]
        is_error: bool,
    },
    /// A harness-level system/event record (mode change, lifecycle, injected
    /// instructions) — also a process quantity.
    System { subtype: String, text: String },
    /// Store metadata the model does not consume as conversation content:
    /// session_meta, indexes, usage counters, titles, file snapshots.
    Meta {
        subtype: String,
        data: serde_json::Value,
    },
}

impl Body {
    pub fn kind(&self) -> RecordKind {
        match self {
            Body::User { .. } => RecordKind::User,
            Body::Assistant { .. } => RecordKind::Assistant,
            Body::Reasoning { .. } => RecordKind::Reasoning,
            Body::ToolCall { .. } => RecordKind::ToolCall,
            Body::ToolResult { .. } => RecordKind::ToolResult,
            Body::System { .. } => RecordKind::System,
            Body::Meta { .. } => RecordKind::Meta,
        }
    }

    /// Flat text used by search and redaction previews.
    pub fn text(&self) -> String {
        match self {
            Body::User { text, .. } | Body::Assistant { text, .. } => text.clone(),
            Body::Reasoning { text } => text.clone(),
            Body::ToolCall { name, args, .. } => format!("{name} {}", args),
            Body::ToolResult { content, .. } => match content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            Body::System { subtype, text } => format!("{subtype}: {text}"),
            Body::Meta { subtype, data } => format!("{subtype}: {data}"),
        }
    }
}

/// Non-text payload attached to a user turn (image, file, blob reference).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Native payload (base64 source block, path reference, …).
    pub data: serde_json::Value,
}

/// Kinds as named members of the superset — selector and matrix currency.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordKind {
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
    System,
    Meta,
}

impl RecordKind {
    pub const ALL: [RecordKind; 7] = [
        RecordKind::User,
        RecordKind::Assistant,
        RecordKind::Reasoning,
        RecordKind::ToolCall,
        RecordKind::ToolResult,
        RecordKind::System,
        RecordKind::Meta,
    ];

    pub fn name(self) -> &'static str {
        match self {
            RecordKind::User => "user",
            RecordKind::Assistant => "assistant",
            RecordKind::Reasoning => "reasoning",
            RecordKind::ToolCall => "tool-call",
            RecordKind::ToolResult => "tool-result",
            RecordKind::System => "system",
            RecordKind::Meta => "meta",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}
