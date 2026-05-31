#![forbid(rustdoc::all, unsafe_code, clippy::pedantic)]
//! The Mirage daemon.

pub mod filesystem;

pub mod state;

pub mod manifest;

pub mod server;

pub mod background;
