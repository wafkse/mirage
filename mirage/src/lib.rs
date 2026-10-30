#![forbid(rustdoc::all, unsafe_code)]
// NOTE: `deny` permits lint allowances emitted by derive macros.
#![deny(clippy::pedantic)]
//! Render application configuration from TOML profiles and `MiniJinja` templates.
//!
//! Use [`server::Mirage`] to run a daemon or render templates once. Extend templates with
//! functions, filters, and globals through [`lua`].

pub mod filesystem;

pub mod lua;

pub mod state;

pub mod manifest;

pub mod server;

pub mod background;
