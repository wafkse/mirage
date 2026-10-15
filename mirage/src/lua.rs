//! Luau module loading and value conversion for `MiniJinja` templates.
//!
//! [`LuaRuntime`] evaluates the configured module and registers its exported functions, filters,
//! and globals alongside `MiniJinja`'s built-ins. File modules resolve within the configure root.
//! Host modules under `@mirage/` require permission in [`ManifestModule::libraries`].
//!
//! Template arguments enter Luau as [`MirageValue`] userdata with lazy lookups. Returning that
//! userdata preserves the underlying `MiniJinja` value. Native Luau return values are converted
//! through [`MirageValue::try_from`], which checks table depth, cycles, and map keys.

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use eyre::eyre;

use minijinja::{Environment, Error, ErrorKind, value::Rest};

use mlua::{IntoLua, Lua, MetaMethod, UserData, UserDataMethods, Variadic};

use tokio::runtime::Runtime;

use crate::manifest::ManifestModule;

/// Exports from `function_table`, `filter_table`, and `environment_table`, grouped for registration.
type ModuleExports = (
    Vec<(String, mlua::Function)>,
    Vec<(String, mlua::Function)>,
    Vec<(String, minijinja::Value)>,
);

/// A lazily-resolving view of a [`minijinja::Value`] handed to Luau as userdata.
///
/// Defined arguments passed to a module function or filter use this wrapper. Indexing resolves
/// the requested child lazily and preserves `MiniJinja`'s zero-based sequence indices. The methods
/// `kind`, `get`, `undefined`, `none`, and `materialize` take precedence over data keys. Use
/// `value:get(key)` to reach a key that shares a method name.
#[derive(Debug, Clone)]
pub struct MirageValue(pub minijinja::Value);

impl MirageValue {
    /// Consume the wrapper and return its template value.
    #[inline]
    #[must_use]
    pub fn into_value(self) -> minijinja::Value {
        let Self(target_value) = self;

        target_value
    }

    /// Construct the function backing a reserved [`MirageValue`] method name.
    ///
    /// Each closure captures the wrapped value. The `get` closure accepts the implicit receiver
    /// followed by the lookup key.
    fn method(&self, target_lua: &Lua, method_name: &str) -> mlua::Result<mlua::Value> {
        let Self(target_value) = self;

        let target_value = target_value.clone();

        let target_function = match method_name {
            "kind" => {
                let target_value = target_value.kind().to_string();

                target_lua.create_function(move |_, (): ()| Ok(target_value.clone()))
            }
            "undefined" => {
                let target_state = target_value.is_undefined();

                target_lua.create_function(move |_, (): ()| Ok(target_state))
            }
            "none" => {
                let target_state = target_value.is_none();

                target_lua.create_function(move |_, (): ()| Ok(target_state))
            }
            "materialize" => target_lua.create_function(move |target_lua, (): ()| {
                Self(target_value.clone()).materialize(target_lua)
            }),
            // NOTE: `get` reaches data keys that share a reserved method name.
            _ => target_lua.create_function(
                move |target_lua, (_, key): (mlua::Value, mlua::Value)| {
                    Self(target_value.clone()).data_index(target_lua, &key)
                },
            ),
        }?;

        target_function.into_lua(target_lua)
    }

    /// Perform a data lookup against a [`minijinja::Value`], mapping a missing reference to Lua `nil`.
    ///
    /// String keys resolve attributes and integer keys resolve items using `MiniJinja`'s indices.
    /// Unsupported keys and undefined results yield `nil`. A `none` value retains its [`MirageValue`]
    /// wrapper and can be tested with `:none()`.
    fn data_index(&self, target_lua: &Lua, key: &mlua::Value) -> mlua::Result<mlua::Value> {
        let Self(target_value) = self;

        let resolved = match key {
            mlua::Value::Integer(index) => target_value.get_item(&minijinja::Value::from(*index)),
            mlua::Value::String(attribute) => {
                let attribute = attribute.to_str()?;

                target_value.get_attr(&attribute)
            }
            _ => return Ok(mlua::Value::Nil),
        }
        .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

        if resolved.is_undefined() {
            Ok(mlua::Value::Nil)
        } else {
            MirageValue(resolved).into_lua(target_lua)
        }
    }

    /// Snapshot a value for Luau generalized iteration through `__iter`.
    ///
    /// Sequences yield one-based positional keys. Maps yield keys converted to native Lua scalars.
    /// The returned iterator walks this eagerly captured snapshot.
    fn pairs_snapshot(
        &self,
        target_lua: &Lua,
    ) -> mlua::Result<Vec<(mlua::Value, minijinja::Value)>> {
        use minijinja::value::ValueKind;

        let Self(target_value) = self;

        let mut snapshot = Vec::new();

        match target_value.kind() {
            ValueKind::Seq | ValueKind::Iterable => {
                let target_iterator = target_value
                    .try_iter()
                    .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                for (position, target_item) in target_iterator.enumerate() {
                    let position = i64::try_from(position)
                        .map(|position| position + 1)
                        .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                    snapshot.push((mlua::Value::Integer(position), target_item));
                }
            }
            ValueKind::Map => {
                let target_iterator = target_value
                    .try_iter()
                    .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                for target_key in target_iterator {
                    let target_item = target_value
                        .get_item(&target_key)
                        .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                    snapshot.push((Self::key_to_lua(target_lua, &target_key)?, target_item));
                }
            }
            _ => {}
        }

        Ok(snapshot)
    }

    /// Lower a minijinja map key to a native Lua scalar for iteration.
    ///
    /// Integer keys become Lua integers. Other keys become strings.
    fn key_to_lua(target_lua: &Lua, target_key: &minijinja::Value) -> mlua::Result<mlua::Value> {
        if let Some(target_integer) = target_key.as_i64() {
            Ok(mlua::Value::Integer(target_integer))
        } else if let Some(target_string) = target_key.as_str() {
            target_lua
                .create_string(target_string)
                .map(mlua::Value::String)
        } else {
            target_lua
                .create_string(target_key.to_string())
                .map(mlua::Value::String)
        }
    }

    /// Deeply convert a [`minijinja::Value`] into native Lua values, backing the `:materialize()` reserved method.
    ///
    /// Sequences become one-based tables and maps retain their converted keys. Undefined and `none`
    /// values become `nil`. Unsupported `MiniJinja` value kinds produce an error.
    fn materialize(&self, target_lua: &Lua) -> mlua::Result<mlua::Value> {
        use minijinja::value::ValueKind;

        let Self(target_value) = self;

        match target_value.kind() {
            ValueKind::Undefined | ValueKind::None => Ok(mlua::Value::Nil),
            ValueKind::Bool => Ok(mlua::Value::Boolean(target_value.is_true())),
            ValueKind::Number => {
                if let Some(target_integer) = target_value.as_i64() {
                    Ok(mlua::Value::Integer(target_integer))
                } else {
                    Ok(mlua::Value::Number(
                        target_value.to_string().parse::<f64>().unwrap_or(f64::NAN),
                    ))
                }
            }
            ValueKind::String => target_lua
                .create_string(target_value.as_str().unwrap_or_default())
                .map(mlua::Value::String),
            ValueKind::Bytes => target_lua
                .create_string(target_value.as_bytes().unwrap_or_default())
                .map(mlua::Value::String),
            ValueKind::Seq | ValueKind::Iterable => {
                let target_table = target_lua.create_table()?;

                let target_iterator = target_value
                    .try_iter()
                    .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                for (position, target_item) in target_iterator.enumerate() {
                    let position = i64::try_from(position)
                        .map(|position| position + 1)
                        .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                    target_table.set(position, Self(target_item).materialize(target_lua)?)?;
                }

                Ok(mlua::Value::Table(target_table))
            }
            ValueKind::Map => {
                let target_table = target_lua.create_table()?;

                let target_iterator = target_value
                    .try_iter()
                    .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                for target_key in target_iterator {
                    let target_item = target_value
                        .get_item(&target_key)
                        .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

                    target_table.set(
                        Self(target_key).materialize(target_lua)?,
                        Self(target_item).materialize(target_lua)?,
                    )?;
                }

                Ok(mlua::Value::Table(target_table))
            }
            _ => Err(mlua::Error::runtime(
                "value cannot be converted to a native Lua value",
            )),
        }
    }
}

impl UserData for MirageValue {
    fn add_methods<M: UserDataMethods<Self>>(target_methods: &mut M) {
        target_methods.add_meta_method(
            MetaMethod::Index,
            |target_lua, target_self, key: mlua::Value| match &key {
                mlua::Value::String(method_name) => match method_name.to_str() {
                    Ok(method_name)
                        if matches!(
                            method_name.as_ref(),
                            "kind" | "get" | "undefined" | "none" | "materialize"
                        ) =>
                    {
                        target_self.method(target_lua, method_name.as_ref())
                    }
                    _ => target_self.data_index(target_lua, &key),
                },
                _ => target_self.data_index(target_lua, &key),
            },
        );

        target_methods.add_meta_method(MetaMethod::Len, |_, target_self, ()| {
            let Self(target_value) = target_self;

            target_value
                .len()
                .and_then(|target_length| i64::try_from(target_length).ok())
                .ok_or_else(|| mlua::Error::runtime("value has no length"))
        });

        target_methods.add_meta_method(MetaMethod::ToString, |_, target_self, ()| {
            let Self(target_value) = target_self;

            Ok(target_value.to_string())
        });

        target_methods.add_meta_method(MetaMethod::Eq, |_, target_self, other: mlua::Value| {
            let Self(target_value) = target_self;

            match other {
                mlua::Value::UserData(other) => Ok(other.borrow::<Self>().is_ok_and(|other| {
                    let Self(other_value) = &*other;

                    target_value == other_value
                })),
                _ => Ok(false),
            }
        });

        // NOTE: Luau generalized iteration calls `__iter`.
        target_methods.add_meta_method(MetaMethod::Iter, |target_lua, target_self, ()| {
            let snapshot = target_self.pairs_snapshot(target_lua)?;

            let cursor = std::cell::Cell::new(0_usize);

            let target_iterator = target_lua.create_function(move |target_lua, (): ()| {
                let position = cursor.get();

                match snapshot.get(position) {
                    Some((target_key, target_value)) => {
                        cursor.set(position + 1);

                        let target_key = target_key.clone();
                        let target_value = Self(target_value.clone()).into_lua(target_lua)?;

                        Ok((target_key, target_value))
                    }
                    None => Ok((mlua::Value::Nil, mlua::Value::Nil)),
                }
            })?;

            Ok((target_iterator, mlua::Value::Nil, mlua::Value::Nil))
        });
    }
}

impl From<&toml::Value> for MirageValue {
    /// Convert TOML data recursively, preserving scalar kinds and formatting datetimes as text.
    #[inline]
    fn from(target_value: &toml::Value) -> Self {
        let target_value = match target_value {
            toml::Value::String(target_value) => minijinja::Value::from(target_value.clone()),
            toml::Value::Integer(target_value) => minijinja::Value::from(*target_value),
            toml::Value::Float(target_value) => minijinja::Value::from(*target_value),
            toml::Value::Boolean(target_value) => minijinja::Value::from(*target_value),
            toml::Value::Datetime(target_value) => minijinja::Value::from(target_value.to_string()),
            toml::Value::Array(target_list) => minijinja::Value::from(
                target_list
                    .iter()
                    .map(|target_value| Self::from(target_value).into_value())
                    .collect::<Vec<_>>(),
            ),
            toml::Value::Table(target_table) => target_table
                .iter()
                .map(|(target_key, target_value)| {
                    (target_key.clone(), Self::from(target_value).into_value())
                })
                .collect(),
        };

        Self(target_value)
    }
}

impl TryFrom<mlua::Value> for MirageValue {
    type Error = Error;

    /// Convert Luau return data, checking table depth, cycles, and key kinds.
    #[inline]
    fn try_from(target_value: mlua::Value) -> Result<Self, Self::Error> {
        ValueConversion::default()
            .convert_value(target_value, 0)
            .map(Self)
    }
}

/// State for recursive conversion of native Luau tables into template values.
// NOTE(invariant): `visited` contains the table identities on the active conversion path.
#[derive(Debug, Default)]
struct ValueConversion {
    /// Table identities used to reject cycles in the current recursion path.
    visited: HashSet<*const c_void>,
}

impl ValueConversion {
    /// Maximum number of nested Luau tables accepted during conversion.
    const MAX_TABLE_DEPTH: usize = 128;

    /// Build a render-time engine error carrying the provided message.
    #[inline]
    fn render_error(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::InvalidOperation, message.into())
    }

    /// Convert a Luau failure into a `MiniJinja` rendering error.
    #[inline]
    fn into_render_error(target_error: &mlua::Error) -> Error {
        Self::render_error(target_error.to_string())
    }

    /// The recursive worker behind [`MirageValue::try_from`], threading the depth budget and cycle-detection set.
    fn convert_value(
        &mut self,
        target_value: mlua::Value,
        depth: usize,
    ) -> Result<minijinja::Value, Error> {
        match target_value {
            mlua::Value::Nil => Ok(minijinja::Value::from(())),
            mlua::Value::Boolean(target_value) => Ok(minijinja::Value::from(target_value)),
            mlua::Value::Integer(target_value) => Ok(minijinja::Value::from(target_value)),
            mlua::Value::Number(target_value) => Ok(minijinja::Value::from(target_value)),
            mlua::Value::String(target_value) => match target_value.to_str() {
                Ok(target_value) => Ok(minijinja::Value::from(String::from(&*target_value))),
                Err(..) => Ok(minijinja::Value::from_bytes(
                    target_value.as_bytes().to_vec(),
                )),
            },
            mlua::Value::Table(target_table) => self.convert_table(&target_table, depth),
            mlua::Value::UserData(target_userdata) => {
                if let Ok(target_value) = target_userdata.borrow::<MirageValue>() {
                    let MirageValue(target_value) = &*target_value;

                    Ok(target_value.clone())
                } else {
                    Err(Self::render_error(
                        "foreign userdata cannot be converted to a value",
                    ))
                }
            }
            mlua::Value::Function(..) => Err(Self::render_error(
                "a Luau function cannot be returned as template data",
            )),
            mlua::Value::Vector(..)
            | mlua::Value::Thread(..)
            | mlua::Value::LightUserData(..)
            | mlua::Value::Error(..)
            | mlua::Value::Buffer(..)
            | mlua::Value::Other(..) => Err(Self::render_error(
                "value cannot be converted to template data",
            )),
        }
    }

    /// Convert a Luau table while retaining its identity only for the active recursion path.
    fn convert_table(
        &mut self,
        target_table: &mlua::Table,
        depth: usize,
    ) -> Result<minijinja::Value, Error> {
        if depth >= Self::MAX_TABLE_DEPTH {
            Err(Self::render_error(
                "maximum table conversion depth exceeded",
            ))
        } else {
            let Self { visited } = self;

            let identity = target_table.to_pointer();

            if visited.insert(identity) {
                let target_result = self.table_value(target_table, depth);

                let Self { visited } = self;

                let _ = visited.remove(&identity);

                target_result
            } else {
                Err(Self::render_error(
                    "a cyclic table cannot be converted to a value",
                ))
            }
        }
    }

    /// Convert the admitted table's entries into a sequence or a map.
    fn table_value(
        &mut self,
        target_table: &mlua::Table,
        depth: usize,
    ) -> Result<minijinja::Value, Error> {
        let entries = target_table
            .pairs::<mlua::Value, mlua::Value>()
            .collect::<mlua::Result<Vec<_>>>()
            .map_err(|target_error| Self::into_render_error(&target_error))?;

        match (entries.as_slice(), Self::as_sequence(&entries)) {
            ([], _) => Ok(minijinja::Value::from(Vec::<minijinja::Value>::new())),
            (_, Some(target_sequence)) => {
                let mut target_list = Vec::with_capacity(target_sequence.len());

                for target_item in target_sequence {
                    target_list.push(self.convert_value(target_item.clone(), depth + 1)?);
                }

                Ok(minijinja::Value::from(target_list))
            }
            (entries, None) => {
                let mut target_pairs = Vec::with_capacity(entries.len());

                for (target_key, target_item) in entries {
                    let target_key = Self::convert_key(target_key)?;
                    let target_item = self.convert_value(target_item.clone(), depth + 1)?;

                    target_pairs.push((target_key, target_item));
                }

                Ok(target_pairs.into_iter().collect())
            }
        }
    }

    /// Decide whether a table's entries form an array (keys exactly `1..=n`), returning the values in order if so.
    fn as_sequence<'a>(entries: &'a [(mlua::Value, mlua::Value)]) -> Option<Vec<&'a mlua::Value>> {
        let count = entries.len();

        let mut ordered: Vec<Option<&'a mlua::Value>> = vec![None; count];

        for (target_key, target_value) in entries {
            let &mlua::Value::Integer(index) = target_key else {
                return None;
            };

            let slot = usize::try_from(index).ok()?;

            if slot < 1 || slot > count {
                return None;
            }

            let target_slot = ordered.get_mut(slot - 1)?;

            if target_slot.is_some() {
                return None;
            }

            *target_slot = Some(target_value);
        }

        ordered.into_iter().collect()
    }

    /// Convert a Luau table key into a [`minijinja::Value`] map key.
    ///
    /// Accepts integer and UTF-8 string keys. Other key kinds produce an error.
    fn convert_key(target_key: &mlua::Value) -> Result<minijinja::Value, Error> {
        match target_key {
            mlua::Value::Integer(target_key) => Ok(minijinja::Value::from(*target_key)),
            mlua::Value::String(target_key) => match target_key.to_str() {
                Ok(target_key) => Ok(minijinja::Value::from(String::from(&*target_key))),
                Err(..) => Err(Self::render_error(
                    "a non-UTF-8 string is not a valid map key",
                )),
            },
            _ => Err(Self::render_error("unsupported map key type")),
        }
    }
}

/// A Luau function paired with the Tokio runtime that drives its asynchronous calls.
// NOTE(invariant): None.
#[derive(Debug)]
struct LuaCallable {
    /// The module function invoked by a template function or filter.
    target_function: mlua::Function,

    /// The current-thread runtime owned by the rendering worker.
    target_runtime: Arc<Runtime>,
}

impl LuaCallable {
    /// Capture a function and the runtime used to complete its asynchronous calls.
    const fn new(target_function: mlua::Function, target_runtime: Arc<Runtime>) -> Self {
        Self {
            target_function,
            target_runtime,
        }
    }

    /// Drive a stored module callable from a minijinja function or filter invocation.
    ///
    /// A filter's piped value leads the argument list. Defined arguments use lazy [`MirageValue`]
    /// wrappers and undefined arguments become `nil`. The return value crosses back through
    /// [`MirageValue::try_from`].
    ///
    /// The worker runtime drives `call_async` to completion during synchronous rendering.
    fn invoke(
        &self,
        piped_value: Option<minijinja::Value>,
        target_args: Vec<minijinja::Value>,
    ) -> Result<minijinja::Value, Error> {
        let Self {
            target_runtime,
            target_function,
        } = self;

        let mut lua_args: Vec<Option<MirageValue>> = Vec::with_capacity(target_args.len() + 1);

        lua_args.extend(
            piped_value
                .into_iter()
                .chain(target_args)
                .map(|target_value| {
                    if target_value.is_undefined() {
                        None
                    } else {
                        Some(MirageValue(target_value))
                    }
                }),
        );

        // NOTE: Rendering starts after the event wait's `block_on` has finished.
        let target_return = target_runtime
            .block_on(target_function.call_async::<mlua::Value>(Variadic::from(lua_args)))
            .map_err(|target_error| ValueConversion::into_render_error(&target_error))?;

        MirageValue::try_from(target_return).map(MirageValue::into_value)
    }
}

/// The per-VM state backing the host `require` implementation.
///
/// The directory stack selects the base for relative imports during module evaluation. Caches
/// retain successful file evaluations and host-library instances for the lifetime of the VM.
// NOTE(invariant): None.
#[derive(Debug, Default)]
struct RequireState {
    /// Directories of modules whose evaluation is in progress.
    stack: Vec<PathBuf>,

    /// The per-VM file-module cache, keyed by canonicalized module path.
    cache: HashMap<PathBuf, mlua::Value>,

    /// The per-VM cache of instantiated `@mirage/<lib>` modules, keyed by library name.
    builtins: HashMap<String, mlua::Value>,
}

/// Per-VM file resolution, host-library permissions, and import caches.
// NOTE(invariant): `root` is canonical and file cache keys lie within it.
#[derive(Debug)]
struct ModuleLoader {
    /// The canonical configure root bounding file imports.
    root: PathBuf,

    /// Host-library names permitted by the manifest.
    libraries: Vec<String>,

    /// Evaluation directories and cached module values.
    state: Mutex<RequireState>,
}

impl ModuleLoader {
    /// Canonicalize the configure root and retain the manifest's host-library permissions.
    fn new(configure_root: &Path, module_manifest: &ManifestModule) -> eyre::Result<Self> {
        let root = configure_root.canonicalize()?;

        let ManifestModule { libraries } = module_manifest;

        let libraries = libraries.clone();

        let state = Mutex::new(RequireState::default());

        Ok(Self {
            root,
            libraries,
            state,
        })
    }

    /// The host `require` implementation, shared by the top-level module load and in-module requires.
    ///
    /// File imports resolve against the requiring file's directory, or the configure root for the
    /// entry module. Canonical paths must remain within that root. `@mirage/` imports use the host
    /// library allowlist and cache.
    fn require(&self, target_lua: &Lua, target_spec: &str) -> mlua::Result<mlua::Value> {
        let Self {
            root,
            libraries: _,
            state,
        } = self;

        if let Some(target_library) = target_spec.strip_prefix("@mirage/") {
            return self.host(target_lua, target_library);
        }

        let require_base = {
            let state = state.lock().expect("require state poisoned");

            let RequireState { stack, .. } = &*state;

            stack.last().cloned().unwrap_or_else(|| root.clone())
        };

        let resolved_path = Self::resolve(&require_base, target_spec)
            .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

        if !resolved_path.starts_with(root) {
            return Err(mlua::Error::runtime(format!(
                "module `{target_spec}` escapes the configure root"
            )));
        }

        let cached = {
            let state = state.lock().expect("require state poisoned");

            let RequireState { cache, .. } = &*state;

            cache.get(&resolved_path).cloned()
        };

        if let Some(target_value) = cached {
            return Ok(target_value);
        }

        let target_source = std::fs::read_to_string(&resolved_path)
            .map_err(|target_error| mlua::Error::runtime(target_error.to_string()))?;

        {
            let target_parent = resolved_path
                .parent()
                .map_or_else(|| root.clone(), Path::to_path_buf);

            let mut state = state.lock().expect("require state poisoned");

            let RequireState { stack, .. } = &mut *state;

            stack.push(target_parent);
        }

        let target_evaluated = target_lua
            .load(target_source)
            .set_name(resolved_path.to_string_lossy().into_owned())
            .eval::<mlua::Value>();

        {
            let mut state = state.lock().expect("require state poisoned");

            let RequireState { stack, .. } = &mut *state;

            let _ = stack.pop();
        }

        let target_value = target_evaluated?;

        {
            let mut state = state.lock().expect("require state poisoned");

            let RequireState { cache, .. } = &mut *state;

            let _ = cache.insert(resolved_path, target_value.clone());
        }

        Ok(target_value)
    }

    /// Resolve a `@mirage/<lib>` require to a Mirage-provided module, gated by the manifest allowlist.
    ///
    /// Check the allowlist before looking up the library. Successful instances are cached for the VM's lifetime.
    fn host(&self, target_lua: &Lua, target_library: &str) -> mlua::Result<mlua::Value> {
        let Self {
            libraries, state, ..
        } = self;

        if !libraries.iter().any(|allowed| allowed == target_library) {
            return Err(mlua::Error::runtime(format!(
                "`@mirage/{target_library}` is not permitted by the [module].libraries allowlist"
            )));
        }

        let cached = {
            let state = state.lock().expect("require state poisoned");

            let RequireState { builtins, .. } = &*state;

            builtins.get(target_library).cloned()
        };

        if let Some(target_value) = cached {
            return Ok(target_value);
        }

        let Some(target_module) =
            HostLibrary::resolve(target_library).map(|library| library.build(target_lua))
        else {
            return Err(mlua::Error::runtime(format!(
                "unknown Mirage module `@mirage/{target_library}`"
            )));
        };

        let target_value = mlua::Value::Table(target_module?);

        {
            let mut state = state.lock().expect("require state poisoned");

            let RequireState { builtins, .. } = &mut *state;

            let _ = builtins.insert(target_library.to_string(), target_value.clone());
        }

        Ok(target_value)
    }

    /// Resolve a module specifier to a concrete, canonicalized file, mirroring `require` resolution.
    ///
    /// A directory uses `index.luau`, then `init.luau`. An existing file resolves directly. Otherwise,
    /// try appending `.luau` to the specifier. Absolute specifiers supply their own base path.
    fn resolve(require_base: &Path, target_spec: &str) -> eyre::Result<PathBuf> {
        let target_candidate = require_base.join(target_spec);

        if target_candidate.is_dir() {
            for entry_point in ["index.luau", "init.luau"] {
                let target_entry = target_candidate.join(entry_point);

                if target_entry.is_file() {
                    return Ok(target_entry.canonicalize()?);
                }
            }

            return Err(eyre!(
                "module directory `{}` has no index.luau or init.luau",
                target_candidate.display()
            ));
        }

        if target_candidate.is_file() {
            return Ok(target_candidate.canonicalize()?);
        }

        let target_extended = {
            let mut target_value = target_candidate.clone().into_os_string();

            target_value.push(".luau");

            PathBuf::from(target_value)
        };

        if target_extended.is_file() {
            return Ok(target_extended.canonicalize()?);
        }

        Err(eyre!(
            "could not resolve module `{target_spec}` under `{}`",
            require_base.display()
        ))
    }
}

/// Mirage host libraries that can be selected by a permitted import name.
#[derive(Debug, Clone, Copy)]
enum HostLibrary {
    /// Asynchronous timing operations.
    Time,
}

impl HostLibrary {
    /// Resolve a supported host-library name.
    fn resolve(target_library: &str) -> Option<Self> {
        match target_library {
            "time" => Some(Self::Time),
            _ => None,
        }
    }

    /// Build the selected library's Luau export table.
    fn build(self, target_lua: &Lua) -> mlua::Result<mlua::Table> {
        match self {
            Self::Time => {
                let target_table = target_lua.create_table()?;

                let target_sleep =
                    target_lua.create_async_function(|_, target_seconds: f64| async move {
                        tokio::time::sleep(Duration::from_secs_f64(target_seconds.max(0.0))).await;

                        Ok(())
                    })?;

                target_table.set("sleep", target_sleep)?;

                Ok(target_table)
            }
        }
    }
}

/// The Luau scripting runtime owning the module VM and its extracted registrations.
///
/// Retains the evaluated module's VM, functions, filters, and global values. Each render registers
/// these exports in a fresh `MiniJinja` environment. Reloading constructs a new runtime.
// NOTE(invariant): None.
#[derive(Debug)]
pub struct LuaRuntime {
    /// The Luau virtual machine. Held to keep the extracted callables alive.
    _target_lua: Lua,

    /// Functions exported through `function_table` and registered with [`Environment::add_function`].
    target_function_table: Vec<(String, mlua::Function)>,

    /// Filters exported through `filter_table` and registered with [`Environment::add_filter`].
    target_filter_table: Vec<(String, mlua::Function)>,

    /// Globals exported through `environment_table` and registered with [`Environment::add_global`].
    target_environment_table: Vec<(String, minijinja::Value)>,
}

impl LuaRuntime {
    /// Build a Luau VM, evaluate the manifest-declared module, and extract its registrations.
    ///
    /// Resolve the module path relative to the configure root and enable Luau native code generation.
    /// The [`ManifestModule`] allowlist controls access to `@mirage/` host libraries.
    ///
    /// # Errors
    ///
    /// Returns an error when the configure root cannot be canonicalized, the module cannot be resolved or evaluated, or
    /// an exported value cannot be converted.
    #[inline]
    pub fn load(
        configure_root: impl AsRef<Path>,
        module_path: impl AsRef<Path>,
        module_manifest: &ManifestModule,
    ) -> eyre::Result<Self> {
        let target_lua = Lua::new();

        target_lua.enable_jit(true);

        let loader = Arc::new(ModuleLoader::new(configure_root.as_ref(), module_manifest)?);

        let target_require =
            target_lua.create_function(move |target_lua, target_spec: mlua::String| {
                let target_spec = target_spec.to_str()?;

                loader.require(target_lua, &target_spec)
            })?;

        target_lua
            .globals()
            .set("require", target_require.clone())?;

        let module_spec = module_path.as_ref().to_string_lossy().into_owned();

        let module_value = target_require.call::<mlua::Value>(module_spec)?;

        let (target_function_table, target_filter_table, target_environment_table) =
            Self::extract_exports(&module_value)?;

        Ok(Self {
            _target_lua: target_lua,
            target_function_table,
            target_filter_table,
            target_environment_table,
        })
    }

    /// Register every exported function, filter, and global into the provided [`Environment`].
    ///
    /// Registered callables capture the shared runtime to drive asynchronous Luau functions during
    /// rendering. Registering an export replaces an existing item with the same name.
    ///
    /// # Panics
    ///
    /// A registered callable panics if invoked from within an active Tokio runtime.
    #[inline]
    pub fn register(
        &self,
        target_environment: &mut Environment<'static>,
        target_runtime: &Arc<Runtime>,
    ) {
        let Self {
            target_function_table,
            target_filter_table,
            target_environment_table,
            ..
        } = self;

        for (target_name, target_function) in target_function_table {
            let target_function = target_function.clone();

            let target_runtime = Arc::clone(target_runtime);

            let callable = LuaCallable::new(target_function, target_runtime);

            target_environment.add_function(
                target_name.clone(),
                move |target_args: Rest<minijinja::Value>| {
                    let Rest(target_args) = target_args;

                    callable.invoke(None, target_args)
                },
            );
        }

        for (target_name, target_function) in target_filter_table {
            let target_function = target_function.clone();

            let target_runtime = Arc::clone(target_runtime);

            let callable = LuaCallable::new(target_function, target_runtime);

            target_environment.add_filter(
                target_name.clone(),
                move |target_value: minijinja::Value, target_args: Rest<minijinja::Value>| {
                    let Rest(target_args) = target_args;

                    callable.invoke(Some(target_value), target_args)
                },
            );
        }

        for (target_name, target_value) in target_environment_table {
            target_environment.add_global(target_name.clone(), target_value.clone());
        }
    }

    /// Extract the `function_table`, `filter_table`, and `environment_table` tables from an evaluated module value.
    ///
    /// A non-table module value yields empty exports. Missing export tables yield empty registrations
    /// for that category. Unknown top-level keys are ignored.
    fn extract_exports(module_value: &mlua::Value) -> eyre::Result<ModuleExports> {
        let mut target_function_table = Vec::new();
        let mut target_filter_table = Vec::new();
        let mut target_environment_table = Vec::new();

        if let mlua::Value::Table(module_table) = module_value {
            if let Some(target_table) = module_table.get::<Option<mlua::Table>>("function_table")? {
                for target_pair in target_table.pairs::<String, mlua::Function>() {
                    target_function_table.push(target_pair?);
                }
            }

            if let Some(target_table) = module_table.get::<Option<mlua::Table>>("filter_table")? {
                for target_pair in target_table.pairs::<String, mlua::Function>() {
                    target_filter_table.push(target_pair?);
                }
            }

            if let Some(target_table) =
                module_table.get::<Option<mlua::Table>>("environment_table")?
            {
                for target_pair in target_table.pairs::<String, mlua::Value>() {
                    let (target_name, target_value) = target_pair?;

                    let target_value = MirageValue::try_from(target_value)
                        .map(MirageValue::into_value)
                        .map_err(|target_error| eyre!("{target_error}"))?;

                    target_environment_table.push((target_name, target_value));
                }
            }
        }

        Ok((
            target_function_table,
            target_filter_table,
            target_environment_table,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl LuaRuntime {
        /// Build the current-thread runtime used by registered module callables in tests.
        fn block_runtime() -> Arc<Runtime> {
            Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            )
        }
    }

    #[test]
    fn toml_primitives_keep_their_types() {
        assert_eq!(
            MirageValue::from(&toml::Value::Integer(7)).into_value(),
            minijinja::Value::from(7_i64)
        );

        assert_eq!(
            MirageValue::from(&toml::Value::Float(1.5)).into_value(),
            minijinja::Value::from(1.5_f64)
        );

        assert_eq!(
            MirageValue::from(&toml::Value::Boolean(true)).into_value(),
            minijinja::Value::from(true)
        );

        let target_datetime = "1979-05-27T07:32:00Z"
            .parse::<toml::value::Datetime>()
            .unwrap();

        assert_eq!(
            MirageValue::from(&toml::Value::Datetime(target_datetime)).into_value(),
            minijinja::Value::from("1979-05-27T07:32:00Z")
        );
    }

    #[test]
    fn array_table_converts_to_sequence() {
        let target_lua = Lua::new();

        let target_value: mlua::Value = target_lua.load("return {10, 20, 30}").eval().unwrap();

        let target_value = MirageValue::try_from(target_value).unwrap().into_value();

        assert_eq!(target_value.len(), Some(3));
        assert_eq!(
            target_value.get_item_by_index(0).unwrap(),
            minijinja::Value::from(10_i64)
        );
    }

    #[test]
    fn keyed_table_converts_to_map() {
        let target_lua = Lua::new();

        let target_value: mlua::Value = target_lua
            .load(r#"return { name = "mirage", count = 3 }"#)
            .eval()
            .unwrap();

        let target_value = MirageValue::try_from(target_value).unwrap().into_value();

        assert_eq!(
            target_value.get_attr("name").unwrap(),
            minijinja::Value::from("mirage")
        );
        assert_eq!(
            target_value.get_attr("count").unwrap(),
            minijinja::Value::from(3_i64)
        );
    }

    #[test]
    fn empty_table_converts_to_empty_sequence() {
        let target_lua = Lua::new();

        let target_value: mlua::Value = target_lua.load("return {}").eval().unwrap();

        assert_eq!(
            MirageValue::try_from(target_value)
                .unwrap()
                .into_value()
                .len(),
            Some(0)
        );
    }

    #[test]
    fn cyclic_table_is_rejected() {
        let target_lua = Lua::new();

        let target_value: mlua::Value = target_lua
            .load("local a = {}; a.self = a; return a")
            .eval()
            .unwrap();

        assert!(MirageValue::try_from(target_value).is_err());
    }

    #[test]
    fn function_as_data_is_rejected() {
        let target_lua = Lua::new();

        let target_value: mlua::Value = target_lua.load("return function() end").eval().unwrap();

        assert!(MirageValue::try_from(target_value).is_err());
    }

    #[test]
    fn mirage_value_round_trips_by_identity() {
        let target_lua = Lua::new();

        let original = minijinja::Value::from(vec![1_i64, 2, 3]);

        let identity: mlua::Function = target_lua
            .load("return function(target) return target end")
            .eval()
            .unwrap();

        let returned: mlua::Value = identity.call(MirageValue(original.clone())).unwrap();

        assert_eq!(
            MirageValue::try_from(returned).unwrap().into_value(),
            original
        );
    }

    #[test]
    fn lazy_indexing_walks_nested_values() {
        let target_lua = Lua::new();

        let nested = minijinja::context! {
            outer => minijinja::context! { inner => vec![7_i64, 8, 9] }
        };

        let lookup: mlua::Function = target_lua
            .load("return function(target) return target.outer.inner[1] end")
            .eval()
            .unwrap();

        let returned: mlua::Value = lookup.call(MirageValue(nested)).unwrap();

        assert_eq!(
            MirageValue::try_from(returned).unwrap().into_value(),
            minijinja::Value::from(8_i64)
        );
    }

    #[test]
    fn module_function_table_and_environment_table_register() {
        let target_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            target_dir.path().join("mod.luau"),
            "return {\n  function_table = { shout = function(target) return tostring(target):upper() end },\n  environment_table = { brand = \"MIRAGE\" },\n}\n",
        )
        .unwrap();

        let target_runtime = LuaRuntime::load(
            target_dir.path(),
            Path::new("mod.luau"),
            &ManifestModule::default(),
        )
        .unwrap();

        let mut target_environment = Environment::new();

        target_runtime.register(&mut target_environment, &LuaRuntime::block_runtime());

        target_environment
            .add_template("t", "{{ shout(name) }}-{{ brand }}")
            .unwrap();

        let target_output = target_environment
            .get_template("t")
            .unwrap()
            .render(minijinja::context! { name => "hi" })
            .unwrap();

        assert_eq!(target_output, "HI-MIRAGE");
    }

    #[test]
    fn async_module_function_is_driven_to_completion() {
        let target_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            target_dir.path().join("mod.luau"),
            "local time = require(\"@mirage/time\")\nreturn {\n  function_table = { slow = function(target) time.sleep(0.01) return tostring(target):upper() end },\n}\n",
        )
        .unwrap();

        let target_manifest = ManifestModule {
            libraries: vec!["time".to_string()],
        };

        let target_module =
            LuaRuntime::load(target_dir.path(), Path::new("mod.luau"), &target_manifest).unwrap();

        let mut target_environment = Environment::new();

        target_module.register(&mut target_environment, &LuaRuntime::block_runtime());

        target_environment
            .add_template("t", "{{ slow(name) }}")
            .unwrap();

        let target_output = target_environment
            .get_template("t")
            .unwrap()
            .render(minijinja::context! { name => "hi" })
            .unwrap();

        assert_eq!(target_output, "HI");
    }

    #[test]
    fn relative_require_resolves_against_requiring_file() {
        let target_dir = tempfile::tempdir().unwrap();

        let module_root = target_dir.path().join("mymod");

        std::fs::create_dir(&module_root).unwrap();

        std::fs::write(
            module_root.join("index.luau"),
            "local helper = require(\"helper\")\nreturn { environment_table = { greeting = helper.hello } }\n",
        )
        .unwrap();

        std::fs::write(
            module_root.join("helper.luau"),
            "return { hello = \"hey\" }\n",
        )
        .unwrap();

        let target_runtime = LuaRuntime::load(
            target_dir.path(),
            Path::new("mymod"),
            &ManifestModule::default(),
        )
        .unwrap();

        let mut target_environment = Environment::new();

        target_runtime.register(&mut target_environment, &LuaRuntime::block_runtime());

        target_environment
            .add_template("t", "{{ greeting }}")
            .unwrap();

        assert_eq!(
            target_environment
                .get_template("t")
                .unwrap()
                .render(())
                .unwrap(),
            "hey"
        );
    }

    #[test]
    fn mirage_require_is_gated_by_the_allowlist() {
        let target_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            target_dir.path().join("mod.luau"),
            "local _ = require(\"@mirage/time\")\nreturn {}\n",
        )
        .unwrap();

        let target_error = LuaRuntime::load(
            target_dir.path(),
            Path::new("mod.luau"),
            &ManifestModule::default(),
        )
        .unwrap_err();

        assert!(target_error.to_string().contains("not permitted"));
    }

    #[test]
    fn unknown_mirage_module_is_rejected_when_allowlisted() {
        let target_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            target_dir.path().join("mod.luau"),
            "local _ = require(\"@mirage/nope\")\nreturn {}\n",
        )
        .unwrap();

        let target_manifest = ManifestModule {
            libraries: vec!["nope".to_string()],
        };

        let target_error =
            LuaRuntime::load(target_dir.path(), Path::new("mod.luau"), &target_manifest)
                .unwrap_err();

        assert!(target_error.to_string().contains("unknown Mirage module"));
    }

    #[test]
    fn shared_tables_convert_in_separate_branches() {
        let target_lua = Lua::new();

        let target_value = target_lua
            .load("local shared = { value = 7 }; return { left = shared, right = shared }")
            .eval::<mlua::Value>()
            .unwrap();

        let target_value = MirageValue::try_from(target_value).unwrap().into_value();

        assert_eq!(
            target_value.get_attr("left").unwrap(),
            target_value.get_attr("right").unwrap()
        );
        assert_eq!(
            target_value
                .get_attr("left")
                .unwrap()
                .get_attr("value")
                .unwrap(),
            minijinja::Value::from(7_i64)
        );
    }

    #[test]
    fn failed_conversion_releases_table_identities() {
        let target_lua = Lua::new();

        let target_table = target_lua
            .load("return { bad = function() end }")
            .eval::<mlua::Table>()
            .unwrap();

        let mut conversion = ValueConversion::default();

        assert!(
            conversion
                .convert_value(mlua::Value::Table(target_table.clone()), 0)
                .is_err()
        );

        target_table.set("bad", mlua::Value::Nil).unwrap();
        target_table.set("good", 7).unwrap();

        let target_value = conversion
            .convert_value(mlua::Value::Table(target_table), 0)
            .unwrap();

        assert_eq!(
            target_value.get_attr("good").unwrap(),
            minijinja::Value::from(7_i64)
        );
    }

    #[test]
    fn table_depth_limit_accepts_boundary_and_rejects_next_level() {
        let target_lua = Lua::new();

        for depth in [
            ValueConversion::MAX_TABLE_DEPTH,
            ValueConversion::MAX_TABLE_DEPTH + 1,
        ] {
            let mut target_table = target_lua.create_table().unwrap();

            for _ in 1..depth {
                let parent = target_lua.create_table().unwrap();

                parent.set("child", target_table).unwrap();

                target_table = parent;
            }

            let target_result = MirageValue::try_from(mlua::Value::Table(target_table));

            assert_eq!(
                target_result.is_ok(),
                depth == ValueConversion::MAX_TABLE_DEPTH
            );
        }
    }

    #[test]
    fn reserved_method_names_remain_accessible_as_data() {
        let target_lua = Lua::new();

        let target_value = minijinja::context! { kind => "data", materialize => "value" };

        let lookup = target_lua
            .load(
                "return function(value) return { value:get('kind'), value:get('materialize') } end",
            )
            .eval::<mlua::Function>()
            .unwrap();

        let target_result = lookup
            .call::<mlua::Value>(MirageValue(target_value))
            .unwrap();

        let target_result = MirageValue::try_from(target_result).unwrap().into_value();

        assert_eq!(target_result, minijinja::Value::from(vec!["data", "value"]));
    }

    #[test]
    fn materialization_and_generalized_iteration_use_luau_indices() {
        let target_lua = Lua::new();

        let lookup = target_lua
            .load("return function(value) local sum = 0; for key, item in value do sum += key + tonumber(tostring(item)) end; local native = value:materialize(); return { original = tostring(value[0]), first = native[1], sum = sum } end")
            .eval::<mlua::Function>()
            .unwrap();

        let target_value = minijinja::Value::from(vec![10_i64, 20]);

        let target_result = lookup
            .call::<mlua::Value>(MirageValue(target_value))
            .unwrap();

        let target_result = MirageValue::try_from(target_result).unwrap().into_value();

        assert_eq!(
            target_result.get_attr("original").unwrap(),
            minijinja::Value::from("10")
        );
        assert_eq!(
            target_result.get_attr("first").unwrap(),
            minijinja::Value::from(10_i64)
        );
        assert_eq!(
            target_result.get_attr("sum").unwrap(),
            minijinja::Value::from(33_i64)
        );
    }

    #[test]
    fn file_imports_reject_paths_outside_the_configure_root() {
        let target_dir = tempfile::tempdir().unwrap();

        let configure_root = target_dir.path().join("configure");

        std::fs::create_dir(&configure_root).unwrap();
        std::fs::write(target_dir.path().join("outside.luau"), "return {}").unwrap();
        std::fs::write(
            configure_root.join("entry.luau"),
            "return require('../outside')",
        )
        .unwrap();

        let target_result = LuaRuntime::load(
            &configure_root,
            Path::new("entry.luau"),
            &ManifestModule::default(),
        );

        assert!(
            target_result
                .unwrap_err()
                .to_string()
                .contains("escapes the configure root")
        );
    }
}
