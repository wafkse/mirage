//! Hydrate state management.

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

/// The provenance of the Luau scripting module bound to a [`HydrationState`].
///
/// This carries everything the hydration worker needs to (re)build the module VM independently of the rest of the
/// daemon, so a change to module code can rebuild the VM in isolation.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    /// The configure root, used as the `require` jail.
    pub configure_root: PathBuf,

    /// The module path, resolved relative to the configure root.
    pub module_path: PathBuf,

    /// The `[module]` manifest section gating host libraries.
    pub manifest: ManifestModule,
}

/// The hydration state for a set of configure candidates.
#[derive(Debug)]
pub struct HydrationState {
    /// The Luau scripting runtime supplying template function_table and filter_table, when a module is configured.
    lua_runtime: Option<LuaRuntime>,

    /// The provenance used to rebuild [`HydrationState::lua_runtime`] on module changes.
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
    /// When `module_source` is present, the Luau VM is built and its module evaluated eagerly so that any module error
    /// surfaces at startup rather than on the first render.
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
    /// The loop owns a local single-threaded runtime: it awaits the next event by way of `block_on`, then renders
    /// synchronously. Because the event wait has already returned out of `block_on` before a render begins, the async
    /// module callables can themselves be blocked to completion during the render without ever nesting a runtime.
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

            // NOTE: Coalesce a burst of notifications into a single render: keep draining events until the watchers fall
            // quiet for the whole grace period, so the duplicate create/modify/metadata events a single edit emits no
            // longer each trigger their own redundant hydration pass. A zero period opts out entirely.
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
                // NOTE: A module evaluation failure leaves the previous runtime in place; the render below still runs.
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
    /// Build a [`LuaRuntime`] from this module provenance.
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
        // NOTE: Hydration lives on a dedicated OS thread owning a current-thread runtime. This keeps the synchronous,
        // potentially-blocking render — and the async module calls it drives — off the shared multi-thread runtime.
        let (result_send, result_recv) = oneshot::channel();

        let _worker = std::thread::Builder::new()
            .name("mirage-hydrate".to_string())
            .spawn(move || {
                let _ = result_send.send(self.drive());
            })?;

        // The worker loops indefinitely, reporting back only on a terminal error.
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
        // NOTE: Absent a module, this arm never resolves so the worker idles on the other two events.
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
        // NOTE: Any other filesystem event (a directory change, an access, ...) is inert.
        _ => (false, false),
    }
}

/// Construct a fresh [`Environment`] over the current template tree and module registrations.
///
/// This is the minijinja analogue of a full Tera reload: it is cheap, so it is rebuilt on every render rather than being
/// mutated in place. The worker runtime is captured by each module callable so async module code is driven at render
/// time.
fn build_environment(
    target_runtime: &Arc<Runtime>,
    lua_runtime: Option<&LuaRuntime>,
    template_root: &Path,
    undefined: UndefinedKind,
) -> Environment<'static> {
    let mut target_environment = Environment::new();

    // NOTE: A bespoke loader is used in place of `minijinja::path_loader`, which refuses any template whose name begins
    // with a dot or sits under a dot-folder — fatal for a dotfiles manager whose templates routinely live under
    // `.config/` or are themselves dotfiles. The loader owns its root since the environment is `'static`.
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
/// This mirrors `minijinja::path_loader` but drops its rejection of dot-prefixed segments, while preserving the
/// traversal guard so a loader-relative name can never escape the template root.
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

/// Join a loader-relative template name onto its root, rejecting anything that could escape the root.
///
/// Unlike the loader minijinja ships, a leading-dot segment (a dotfile or dot-folder) is permitted; only empty
/// segments, parent traversal, and backslashes are refused.
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

/// Render every template into the temporary directory, returning the pending atomic renames.
///
/// Rendering is all-or-nothing: every template is rendered and staged first, and nothing is renamed unless all succeed,
/// so a mid-render failure leaves the existing outputs untouched.
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

        // NOTE: Strip the template extension from both the source and atomic tempfile path.
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

/// Commit the staged renders by performing the pending atomic renames.
///
/// Each tempfile shares the template's mount, so the rename is atomic and an external watcher never observes a partial
/// write. This is the terminal phase of the all-or-nothing transaction: it runs only once every template has rendered.
fn commit_renames(rename_list: Vec<(PathBuf, PathBuf)>) -> eyre::Result<()> {
    for (from, to) in rename_list {
        fs::rename(from, to)?;
    }

    Ok(())
}

/// Render every template once and commit the result, building a private runtime to drive any async module code.
///
/// This is the synchronous one-shot analogue of the worker's `drive` loop: it owns its own current-thread runtime so a
/// `--oneshot` invocation can hydrate a manifest a single time without standing up the daemon, its watchers, or the
/// control socket.
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

/// Determine whether any path in the event bears the provided extension.
///
/// This generalizes the self-recursion guard: the worker reacts only to events whose paths carry the configured
/// extension, so in-place hydrated output never retriggers a render.
fn paths_bear_extension(path_list: &[PathBuf], target_extension: &str) -> bool {
    path_list.iter().all(|target_path| {
        target_path
            .extension()
            .is_some_and(|target_value| target_value.eq_ignore_ascii_case(target_extension))
    })
}

/// Report an error chain through the tracing subscriber, mirroring the daemon's top-level reporting.
///
/// A non-fatal render or module-evaluation failure is logged rather than propagated, since the all-or-nothing
/// transaction has already left the existing outputs untouched and the worker must keep reacting to later changes.
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
