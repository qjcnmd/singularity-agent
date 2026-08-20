//! Bounded file I/O and append-lock seam for Session JSONL.
//!
//! File mutation is still owned by `SessionManager`; this module exposes the
//! stable public types used by file-oriented callers while the implementation
//! is migrated in later behavior slices.

pub use super::manager::{SessionError, SessionManager};
