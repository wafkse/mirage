//! Timing functions for Luau extensions.
//!
//! See [`Time`] for additional information.

use std::time::Duration;

use mlua::{Lua, Table};

use super::library::Library;

/// Timing functions available through `require("@mirage/time")`.
pub struct Time;

impl Library for Time {
    #[inline]
    fn build(self, lua: &Lua) -> mlua::Result<Table> {
        let table = lua.create_table()?;
        let sleep = lua.create_async_function(|_, seconds: f64| async move {
            tokio::time::sleep(Duration::from_secs_f64(seconds.max(0.0))).await;

            Ok(())
        })?;

        table.set("sleep", sleep)?;

        Ok(table)
    }
}
