//! ACO — Agent Context Operators.
//!
//! Operators over agent conversation artifacts that already exist on disk:
//! search, list, export, transform, import, edit and snapshot. The crate
//! owns the canonical conversation model (a superset of every agent's
//! native record vocabulary), store discovery over `a4aa`
//! `AgentCard::history`, the [`Codec`] contract with its capability
//! matrix, and the operator engines. Per-agent codecs live in
//! `aco-<agent>` crates; the `aco` binary lives in `aco-cli`.
//!
//! Runtime hook evaluation and live-session injection are the Agent Hook
//! Engine's domain and are deliberately absent here.

pub mod codec;
pub mod edit;
pub mod error;
pub mod model;
pub mod ops;
pub mod search;
pub mod select;
pub mod snapshot;
pub mod store;
pub mod util;

pub use codec::{Capability, CapabilityMatrix, Codec, Decoded, Encoded, Projection, Skipped};
pub use error::{Error, Result};
pub use model::{Attachment, Body, Conversation, Record, RecordKind, FORMAT};
pub use select::{Selector, Selectors};
pub use store::{StoreFile, StoreScan};
