#![forbid(rustdoc::all, unsafe_code, clippy::pedantic)]
//! Shared filesystem, manifest, and daemon state components.

pub mod filesystem;

pub mod state;

pub mod manifest;

pub mod server;

pub mod background;
