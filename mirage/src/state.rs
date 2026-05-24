//! Cooperating event loops for configuration, rendering, and runtime control.
//!
//! [`configure`] publishes the selected TOML context through watch channels. [`control`] changes
//! that context or its profile, and [`hydrate`] renders templates when inputs change.

pub mod hydrate;

pub mod configure;

pub mod control;
