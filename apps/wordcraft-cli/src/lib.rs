//! `wordcraft-cli` as a library: the parts of the command line that its integration tests drive
//! directly (the live co-editing document service).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod live_worker;
