//! Template rendering, staged output replacement, and Luau module reloads.
//!
//! [`HydrationState`] watches template, module, and context changes on a dedicated worker thread.
//! Each pass stages every rendered output before replacing destination files individually.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use eyre::eyre;
use minijinja::{Environment, ErrorKind, UndefinedBehavior};
use notify::{RecursiveMode, Watcher};
use tempfile::{TempDir, tempdir_in};
use tokio::{
    runtime::Runtime,
    sync::{oneshot, watch},
};

use walkdir::WalkDir;

use crate::{
    background::Background,
    filesystem::FsWatcher,
    lua::{self, LuaRuntime},
    manifest::{ManifestModule, UndefinedKind},
};

/// Paths and library permissions used to load a hydration worker's Luau module.
///
/// The worker retains these inputs to build a fresh VM when Luau files change.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    /// The configure root, used as the `require` jail.
    pub configure_root: PathBuf,

    /// The module path, resolved relative to the configure root.
    pub module_path: PathBuf,

    /// The `[module]` manifest section gating host libraries.
    pub manifest: ManifestModule,
}

/// Render templates in response to changes in templates, Luau modules, and the TOML context.
///
/// The worker coalesces input events, stages rendered files, and replaces each destination by
/// rename. A failed module reload retains the previous VM for subsequent rendering.
// NOTE(invariant): None.
#[derive(Debug)]
pub struct HydrationState {
    /// The optional Luau VM supplying template functions, filters, and globals.
    lua_runtime: Option<LuaRuntime>,

    /// The settings used to rebuild [`Self::lua_runtime`] on module changes.
    module_source: Option<ModuleSource>,

    /// The filesystem watcher for *template* files.
    watcher_template: FsWatcher,

    /// The filesystem watcher for *module* files, present only when a module is configured.
    watcher_module: Option<FsWatcher>,

    /// The pipe used for render context management.
    context_pipe: watch::Receiver<toml::Value>,

    /// The render path root with the hydrated templates.
    template_root: PathBuf,

    /// The extension borne by template candidates.
    template_extension: String,

    /// The undefined-reference behaviour applied to the rendering engine.
    undefined: UndefinedKind,

    /// The grace period over which a burst of filesystem notifications is coalesced into a single render.
    notificate_period: Duration,

    /// A boolean that indicates whether the startup hydrate has been performed.
    start_hydrate: bool,
}

/// An event to the hydration subsystem.
#[derive(Debug)]
enum HydrationEvent {
    /// A filesystem event pertaining to the watched templates.
    Template(notify::Event),

    /// A filesystem event pertaining to the watched module tree.
    Module(notify::Event),

    /// The context was updated.
    Context,
}

impl HydrationState {
    /// Instantiate a new hydration state.
    ///
    /// When `module_source` is present, evaluate the module during construction. Register watchers
    /// for the template tree and, when applicable, the configure tree containing Luau modules.
    ///
    /// # Errors
    ///
    /// Returns an error when the module fails to evaluate or a filesystem watcher cannot be established.
    #[inline]
    pub fn new(
        template_root: impl AsRef<Path>,
        context_pipe: watch::Receiver<toml::Value>,
        template_extension: String,
        undefined: UndefinedKind,
        notificate_period: Duration,
        module_source: Option<ModuleSource>,
    ) -> eyre::Result<Self> {
        let lua_runtime = module_source.as_ref().map(ModuleSource::load).transpose()?;

        let mut watcher_template = FsWatcher::standard()?;

        watcher_template.watch(template_root.as_ref(), RecursiveMode::Recursive)?;

        let watcher_module = module_source
            .as_ref()
            .map(|target_source| -> eyre::Result<FsWatcher> {
                let mut target_watch = FsWatcher::standard()?;

                target_watch.watch(
                    target_source.configure_root.as_path(),
                    RecursiveMode::Recursive,
                )?;

                Ok(target_watch)
            })
            .transpose()?;

        let template_root = template_root.as_ref().to_path_buf();

        let start_hydrate = false;

        Ok(Self {
            lua_runtime,
            module_source,
            watcher_template,
            watcher_module,
            context_pipe,
            template_root,
            template_extension,
            undefined,
            notificate_period,
            start_hydrate,
        })
    }

    /// Drive the synchronous hydration loop on the worker thread.
    ///
    /// A current-thread runtime waits for events and drives asynchronous Luau calls. Rendering
    /// runs between event waits, with each `block_on` call finishing before the next begins.
    fn drive(self) -> eyre::Result<()> {
        let Self {
            mut lua_runtime,
            module_source,
            mut watcher_template,
            mut watcher_module,
            mut context_pipe,
            template_root,
            template_extension,
            undefined,
            notificate_period,
            mut start_hydrate,
        } = self;

        let _span = tracing::info_span!("hydrate").entered();

        let target_runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        );

        tracing::info!(
            ?notificate_period,
            template_root = %template_root.display(),
            "hydration worker started",
        );

        loop {
            let target_event = target_runtime.block_on(next_event(
                &mut watcher_template,
                &mut watcher_module,
                &mut context_pipe,
            ));

            let (target_change, mut reload_module) = if start_hydrate {
                classify_event(&target_event, &template_extension)
            } else {
                start_hydrate = true;

                tracing::debug!("performing startup hydrate");

                (true, false)
            };

            if !target_change {
                tracing::trace!(?target_event, "ignoring inert event");

                continue;
            }

            // NOTE: Each received event restarts the quiet interval before rendering.
            if !notificate_period.is_zero() {
                target_runtime.block_on(async {
                    let mut coalesced = 0_usize;

                    while let Ok(extra_event) = tokio::time::timeout(
                        notificate_period,
                        next_event(
                            &mut watcher_template,
                            &mut watcher_module,
                            &mut context_pipe,
                        ),
                    )
                    .await
                    {
                        let (.., extra_reload) = classify_event(&extra_event, &template_extension);

                        reload_module |= extra_reload;

                        coalesced += 1;
                    }

                    if coalesced > 0 {
                        tracing::debug!(coalesced, "coalesced a burst of notifications");
                    }
                });
            }

            if reload_module && let Some(target_source) = module_source.as_ref() {
                // NOTE: Failed module evaluation retains the previous VM for this render.
                tracing::info!("rebuilding module runtime");

                match target_source.load() {
                    Ok(target_module) => lua_runtime = Some(target_module),
                    Err(target_error) => report_error(&target_error),
                }
            }

            let temp_filedir = tempdir_in(template_root.as_path())?;

            let target_value = render(
                &target_runtime,
                lua_runtime.as_ref(),
                template_root.as_path(),
                template_extension.as_str(),
                undefined,
                &context_pipe.borrow(),
                &temp_filedir,
            );

            match target_value {
                Ok(rename_list) => commit_renames(rename_list)?,
                Err(target_error) => report_error(&target_error),
            }
        }
    }
}

impl ModuleSource {
    /// Load a [`LuaRuntime`] from the retained paths and library permissions.
    ///
    /// # Errors
    ///
    /// Returns an error when module resolution, evaluation, or export conversion fails.
    #[inline]
    pub(crate) fn load(&self) -> eyre::Result<LuaRuntime> {
        let Self {
            configure_root,
            module_path,
            manifest,
        } = self;

        LuaRuntime::load(configure_root, module_path, manifest)
    }
}

impl Background for HydrationState {
    type Output = ();

    async fn run(self) -> eyre::Result<Self::Output> {
        // NOTE: The worker owns the current-thread runtime used by rendering and asynchronous Luau calls.
        let (result_send, result_recv) = oneshot::channel();

        let _worker = std::thread::Builder::new()
            .name("mirage-hydrate".to_string())
            .spawn(move || {
                let _ = result_send.send(self.drive());
            })?;

        // NOTE: The worker reports through this channel on termination.
        result_recv.await?
    }
}

/// Await the next hydration event across the template watcher, the optional module watcher, and the context pipe.
async fn next_event(
    watcher_template: &mut FsWatcher,
    watcher_module: &mut Option<FsWatcher>,
    context_pipe: &mut watch::Receiver<toml::Value>,
) -> HydrationEvent {
    let module_event = async {
        // NOTE: With no module watcher, this future remains pending.
        match watcher_module.as_mut() {
            Some(target_watch) => target_watch.receiver_mut().recv().await,
            None => std::future::pending().await,
        }
    };

    tokio::select!(
        Some(target_event) = watcher_template.receiver_mut().recv() => HydrationEvent::Template(target_event),
        Some(target_event) = module_event => HydrationEvent::Module(target_event),
        Ok(..) = context_pipe.changed() => HydrationEvent::Context,
    )
}

/// Classify an event into whether it forces a render and whether it forces a module-VM rebuild.
fn classify_event(target_event: &HydrationEvent, template_extension: &str) -> (bool, bool) {
    match target_event {
        HydrationEvent::Template(notify::Event {
            kind:
                notify::EventKind::Modify(
                    notify::event::ModifyKind::Name(..) | notify::event::ModifyKind::Data(..),
                )
                | notify::EventKind::Create(notify::event::CreateKind::File)
                | notify::EventKind::Remove(notify::event::RemoveKind::File),
            paths: path_list,
            ..
        }) => (paths_bear_extension(path_list, template_extension), false),
        HydrationEvent::Module(notify::Event {
            kind:
                notify::EventKind::Modify(
                    notify::event::ModifyKind::Name(..) | notify::event::ModifyKind::Data(..),
                )
                | notify::EventKind::Create(notify::event::CreateKind::File)
                | notify::EventKind::Remove(notify::event::RemoveKind::File),
            paths: path_list,
            ..
        }) => {
            let target_change = paths_bear_extension(path_list, "luau");

            (target_change, target_change)
        }
        HydrationEvent::Context => (true, false),
        _ => (false, false),
    }
}

/// Construct a fresh [`Environment`] over the current template tree and module registrations.
///
/// Each pass loads current template contents and registers exports from the retained Luau VM.
/// Registered callables capture the worker runtime to drive asynchronous module functions.
fn build_environment(
    target_runtime: &Arc<Runtime>,
    lua_runtime: Option<&LuaRuntime>,
    template_root: &Path,
    undefined: UndefinedKind,
) -> Environment<'static> {
    let mut target_environment = Environment::new();

    // NOTE: The loader accepts dotfiles and owns its root for the environment's `'static` lifetime.
    target_environment.set_loader(file_loader(template_root.to_path_buf()));

    target_environment.set_undefined_behavior(match undefined {
        UndefinedKind::Strict => UndefinedBehavior::Strict,
        UndefinedKind::Lenient => UndefinedBehavior::Lenient,
        UndefinedKind::Chainable => UndefinedBehavior::Chainable,
    });

    if let Some(target_module) = lua_runtime {
        target_module.register(&mut target_environment, target_runtime);
    }

    target_environment
}

/// Build a template loader rooted at the template directory that admits dot paths.
///
/// Names are checked by [`safe_join`] before reading. Filesystem reads follow symbolic links.
fn file_loader(
    template_root: PathBuf,
) -> impl Fn(&str) -> Result<Option<String>, minijinja::Error> + Send + Sync + 'static {
    move |target_name| {
        let Some(target_path) = safe_join(template_root.as_path(), target_name) else {
            return Ok(None);
        };

        match fs::read_to_string(target_path) {
            Ok(target_source) => Ok(Some(target_source)),
            Err(target_error) if target_error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(target_error) => Err(minijinja::Error::new(
                ErrorKind::InvalidOperation,
                "could not read template",
            )
            .with_source(target_error)),
        }
    }
}

/// Join slash-separated template path segments onto the loader root.
///
/// Empty segments, `..`, and backslashes are rejected. Leading-dot filenames are accepted.
/// Validation is lexical and filesystem reads can follow symbolic links outside the root.
fn safe_join(template_root: &Path, target_name: &str) -> Option<PathBuf> {
    let mut target_path = template_root.to_path_buf();

    for target_segment in target_name.split('/') {
        if target_segment.is_empty() || target_segment == ".." || target_segment.contains('\\') {
            return None;
        }

        target_path.push(target_segment);
    }

    Some(target_path)
}

/// Enumerate the loader-relative names of every template candidate under the template root.
fn template_names(template_root: &Path, template_extension: &str) -> eyre::Result<Vec<String>> {
    let mut target_list = Vec::new();

    for target_entry in WalkDir::new(template_root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !target_entry.file_type().is_file() {
            continue;
        }

        let target_path = target_entry.path();

        let target_match = target_path
            .extension()
            .is_some_and(|target_value| target_value.eq_ignore_ascii_case(template_extension));

        if !target_match {
            continue;
        }

        let target_name = target_path
            .strip_prefix(template_root)?
            .to_string_lossy()
            .into_owned();

        target_list.push(target_name);
    }

    Ok(target_list)
}

/// Stage every rendered template and return the pending output renames.
///
/// An error during rendering or staging leaves destination files untouched. Each staged file
/// receives its source template's permissions.
fn render(
    target_runtime: &Arc<Runtime>,
    lua_runtime: Option<&LuaRuntime>,
    template_root: &Path,
    template_extension: &str,
    undefined: UndefinedKind,
    target_context: &toml::Value,
    temp_filedir: &TempDir,
) -> eyre::Result<Vec<(PathBuf, PathBuf)>> {
    let target_environment =
        build_environment(target_runtime, lua_runtime, template_root, undefined);

    let target_context = lua::toml_to_minijinja(target_context);

    let mut rename_list = Vec::new();

    for template_name in template_names(template_root, template_extension)? {
        let mut template_path = template_root.join(template_name.as_str());

        let template_perms = fs::metadata(template_path.as_path())?.permissions();

        let mut temp_path = temp_filedir.path().join(template_name.as_str());

        let _ = template_path.set_extension("");
        let _ = temp_path.set_extension("");

        fs::create_dir_all(
            temp_path
                .parent()
                .ok_or_else(|| eyre!("tempfile does not have a parent"))?,
        )?;

        let rendered_output = target_environment
            .get_template(template_name.as_str())?
            .render(&target_context)?;

        fs::write(temp_path.as_path(), rendered_output)?;

        fs::set_permissions(temp_path.as_path(), template_perms)?;

        tracing::trace!(template = %template_name, "staged template");

        rename_list.push((temp_path, template_path));
    }

    tracing::info!(rendered = rename_list.len(), "render pass staged");

    Ok(rename_list)
}

/// Replace destination files with the staged renders in order.
///
/// Each successful rename atomically replaces its destination on the same filesystem. A failed
/// rename returns an error, leaving earlier replacements in place and later outputs unchanged.
fn commit_renames(rename_list: Vec<(PathBuf, PathBuf)>) -> eyre::Result<()> {
    for (from, to) in rename_list {
        fs::rename(from, to)?;
    }

    Ok(())
}

/// Render every template once and commit the result, building a private runtime to drive any async module code.
///
/// The current-thread runtime drives asynchronous Luau functions during rendering. Output files
/// are replaced individually after every template has rendered successfully.
///
/// # Errors
///
/// Returns an error when the runtime cannot be built, a template fails to render, or a staged rename cannot be committed.
pub(crate) fn render_once(
    lua_runtime: Option<&LuaRuntime>,
    template_root: &Path,
    template_extension: &str,
    undefined: UndefinedKind,
    target_context: &toml::Value,
) -> eyre::Result<()> {
    let _span = tracing::info_span!("oneshot").entered();

    let target_runtime = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    );

    let temp_filedir = tempdir_in(template_root)?;

    let rename_list = render(
        &target_runtime,
        lua_runtime,
        template_root,
        template_extension,
        undefined,
        target_context,
        &temp_filedir,
    )?;

    commit_renames(rename_list)
}

/// Determine whether every path in an event has the supplied extension.
///
/// Extension matching is case insensitive. An empty path list returns `true`.
fn paths_bear_extension(path_list: &[PathBuf], target_extension: &str) -> bool {
    path_list.iter().all(|target_path| {
        target_path
            .extension()
            .is_some_and(|target_value| target_value.eq_ignore_ascii_case(target_extension))
    })
}

/// Log a render or module reload error chain through tracing.
///
/// The worker continues waiting for input changes after these failures.
fn report_error(target_report: &eyre::Report) {
    let target_chain = target_report
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    tracing::error!(causes = ?target_chain, "hydration pass failed");
}

#[cfg(test)]
mod tests {
    use super::{file_loader, safe_join, template_names};

    use std::path::Path;

    use minijinja::Environment;

    #[test]
    fn loader_reads_templates_under_dot_paths() {
        let target_dir = tempfile::tempdir().unwrap();

        std::fs::create_dir(target_dir.path().join(".config")).unwrap();
        std::fs::write(
            target_dir.path().join(".config/app.conf.jinja"),
            "from-dot-folder",
        )
        .unwrap();
        std::fs::write(target_dir.path().join(".bashrc.jinja"), "from-dotfile").unwrap();

        let mut target_environment = Environment::new();

        target_environment.set_loader(file_loader(target_dir.path().to_path_buf()));

        assert_eq!(
            target_environment
                .get_template(".config/app.conf.jinja")
                .unwrap()
                .render(())
                .unwrap(),
            "from-dot-folder"
        );
        assert_eq!(
            target_environment
                .get_template(".bashrc.jinja")
                .unwrap()
                .render(())
                .unwrap(),
            "from-dotfile"
        );
    }

    #[test]
    fn enumeration_discovers_dot_path_templates() {
        let target_dir = tempfile::tempdir().unwrap();

        std::fs::create_dir(target_dir.path().join(".config")).unwrap();
        std::fs::write(target_dir.path().join(".config/app.conf.jinja"), "x").unwrap();
        std::fs::write(target_dir.path().join(".bashrc.jinja"), "x").unwrap();

        let mut target_names = template_names(target_dir.path(), "jinja").unwrap();

        target_names.sort();

        assert_eq!(target_names, [".bashrc.jinja", ".config/app.conf.jinja"]);
    }

    #[test]
    fn safe_join_permits_dots_but_refuses_traversal() {
        let target_root = Path::new("/srv/templates");

        assert_eq!(
            safe_join(target_root, ".config/app.conf.jinja"),
            Some(target_root.join(".config/app.conf.jinja"))
        );

        assert!(safe_join(target_root, "../escape").is_none());
        assert!(safe_join(target_root, "nested/../escape").is_none());
    }
}
