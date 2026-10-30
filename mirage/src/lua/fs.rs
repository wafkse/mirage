//! Filesystem access for Luau extensions.
//!
//! See [`Filesystem`] for additional information.

use std::path::{Path, PathBuf};

use mlua::{Lua, Table};

use super::library::Library;

/// File reads for Luau extensions.
///
/// Relative paths resolve from the configured base directory. Absolute paths and symbolic links
/// can reach any file readable by the process.
// NOTE(invariant): None.
pub struct Filesystem {
    /// Base directory used to resolve relative read paths.
    root: PathBuf,
}

impl Filesystem {
    /// Provide file reads relative to `root`.
    #[inline]
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let root = root.to_path_buf();

        Self { root }
    }
}

impl Library for Filesystem {
    #[inline]
    fn build(self, lua: &Lua) -> mlua::Result<Table> {
        let Self { root } = self;
        let table = lua.create_table()?;
        let read = lua.create_function(move |lua, path: String| {
            let bytes = std::fs::read(root.join(path)).map_err(mlua::Error::external)?;

            lua.create_string(bytes)
        })?;

        table.set("read", read)?;

        Ok(table)
    }
}
