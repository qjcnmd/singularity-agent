//! Session JSONL schema and strict-format façade.
//!
//! The canonical serde implementation remains private to `manager` during the
//! structure-only move.  Re-exporting the public schema here gives future code
//! a stable format seam without introducing a second representation.

pub use super::manager::{
    CompactionEntry, SessionEntry, SessionEntryType, SessionError, SessionMetadata,
    SessionMetadataKind, CURRENT_SESSION_VERSION,
};
