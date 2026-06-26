#![forbid(rustdoc::all, unsafe_code)]
// NOTE: `deny` permits lint allowances emitted by derive macros.
#![deny(clippy::pedantic)]
//! Render application configuration from TOML profiles and `MiniJinja` templates.
//!
//! [`server`] resolves the manifest and starts the configure, hydration, and control subsystems.
//! [`state`] owns their event loops and shared context. [`lua`] supplies Luau template extensions.

pub mod filesystem;

pub mod lua;

pub mod state;

pub mod manifest;

pub mod server;

pub mod background;
