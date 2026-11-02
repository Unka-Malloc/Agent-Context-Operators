//! ACOP — Agent Context Operators.
//!
//! Operators over agent conversation artifacts that already exist on disk:
//! search, list, export, transform, import, edit and snapshot. The crate
//! owns the canonical conversation model (a superset of every agent's
//! native record vocabulary), store discovery over `a4aa`
//! `AgentCard::history`, the [`Codec`] contract with its capability
//! matrix, and the operator engines. Per-agent codecs live in
//! `acop-<agent>` crates; the `acop` binary lives in `acop-cli`.
//!
//! Runtime hook evaluation and live-session injection are the Agent Hook
//! Engine's domain and are deliberately absent here.

pub mod codec;
pub mod edit;
pub mod error;
pub mod generic;
pub mod model;
pub mod ops;
pub mod search;
pub mod select;
pub mod snapshot;
pub mod store;
pub mod util;

pub use codec::{
    Capability, CapabilityMatrix, Codec, Decoded, Encoded, ForkMode, Projection, Skipped,
};
pub use error::{Error, Result};
pub use model::{Attachment, Body, Conversation, ForkOrigin, Record, RecordKind, FORMAT};
pub use select::{Selector, Selectors};
pub use store::{HistoryRoot, SessionEntry, StoreKind, StoreScan, TranscriptUsage};
