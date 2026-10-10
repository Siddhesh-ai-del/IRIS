//! Session durability (phase 2): the versioned JSONL transcript schema
//! (stage 2.1), the crash-safe append writer (2.2), list/resume (2.3),
//! and the rebuildable sqlite index (2.4).

pub mod schema;
pub mod writer;
