//! Host libraries for Luau extensions.
//!
//! See the [`Library`] trait for additional information.

use mlua::{Lua, Table};

/// A host capability that constructs its Luau exports.
///
/// Building a library does not grant import permission or make it available through `require`.
pub trait Library {
    /// Construct the export table in the supplied virtual machine.
    ///
    /// # Errors
    ///
    /// Returns an error if export allocation or library initialization fails.
    fn build(self, lua: &Lua) -> mlua::Result<Table>;
}
