//! Bounded JSON-Lines framing helpers.
//!
//! The implementation currently remains in [`super`] while the transport
//! reorganisation is staged.  This module is the owned seam for the framing
//! contract (one UTF-8 JSON value per line and a hard frame limit).

