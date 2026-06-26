#![forbid(rustdoc::all, unsafe_code)]
// NOTE: `clippy::pedantic` is denied rather than forbidden: `forbid` of a clippy lint group collides with the
// `#[allow]` attributes emitted by derive macros (e.g. clap), which is a `forbidden_lint_groups` future hard error.
#![deny(clippy::pedantic)]
//! The Mirage daemon.

pub mod filesystem;

pub mod lua;

pub mod state;

pub mod manifest;

pub mod server;

pub mod background;
