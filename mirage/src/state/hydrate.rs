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

use eyre::{WrapErr, eyre};
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
    lua::{LuaRuntime, MirageValue},
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
        })
    }

    /// Drive the synchronous hydration loop on the worker thread.
    ///
    /// Render the initial context before waiting for events. A current-thread runtime drives
    /// asynchronous Luau calls and subsequent event waits. Each `block_on` call finishes before
    /// the next begins.
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

        let template_tree = TemplateTree::new(template_root, template_extension);

        let renderer = Renderer::new(
            &target_runtime,
            lua_runtime.as_ref(),
            &template_tree,
            undefined,
        );

        Self::hydrate(&renderer, &context_pipe.borrow())?;

        loop {
            let target_event = target_runtime.block_on(Self::event(
                &mut watcher_template,
                &mut watcher_module,
                &mut context_pipe,
            ));

            let (target_change, mut reload_module) =
                target_event.classify(template_tree.extension());

            if !target_change {
                tracing::trace!(?target_event, "ignoring inert event");

                continue;
            }

            // NOTE: Relevant input changes restart the quiet interval before rendering.
            if !notificate_period.is_zero() {
                target_runtime.block_on(async {
                    let mut coalesced = 0_usize;

                    let mut deadline = tokio::time::Instant::now() + notificate_period;

                    while let Ok(extra_event) = tokio::time::timeout_at(
                        deadline,
                        Self::event(
                            &mut watcher_template,
                            &mut watcher_module,
                            &mut context_pipe,
                        ),
                    )
                    .await
                    {
                        let (extra_change, extra_reload) =
                            extra_event.classify(template_tree.extension());

                        if extra_change {
                            reload_module |= extra_reload;

                            coalesced += 1;

                            deadline = tokio::time::Instant::now() + notificate_period;
                        }

                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }
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
                    Err(target_error) => Self::report(&target_error),
                }
            }

            let renderer = Renderer::new(
                &target_runtime,
                lua_runtime.as_ref(),
                &template_tree,
                undefined,
            );

            Self::hydrate(&renderer, &context_pipe.borrow())?;
        }
    }

    /// Render the current context and retain watcher execution after rendering failures.
    ///
    /// Staging-directory creation and output-replacement errors terminate the worker.
    fn hydrate(renderer: &Renderer<'_>, target_context: &toml::Value) -> eyre::Result<()> {
        let Renderer { template_tree, .. } = renderer;

        let temp_filedir = tempdir_in(template_tree.root())?;

        match renderer.render(target_context, temp_filedir) {
            Ok(staged_render) => staged_render.commit(),
            Err(target_error) => {
                Self::report(&target_error);

                Ok(())
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

impl HydrationState {
    /// Await the next hydration event across the template watcher, the optional module watcher, and the context pipe.
    async fn event(
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

    /// Render every template once and commit the result, building a private runtime to drive any async module code.
    ///
    /// The current-thread runtime drives asynchronous Luau functions during rendering. Output files
    /// are replaced individually after every template has rendered successfully.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot be built, a template fails to render, or a staged rename cannot be committed.
    pub(crate) fn oneshot(
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

        let template_tree = TemplateTree::new(template_root, template_extension);

        let temp_filedir = tempdir_in(template_tree.root())?;

        let renderer = Renderer::new(&target_runtime, lua_runtime, &template_tree, undefined);

        renderer.render(target_context, temp_filedir)?.commit()
    }

    /// Log a render or module reload error chain through tracing.
    ///
    /// The worker continues waiting for input changes after these failures.
    fn report(target_report: &eyre::Report) {
        let target_chain = target_report
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        tracing::error!(causes = ?target_chain, "hydration pass failed");
    }
}

impl HydrationEvent {
    /// Classify an event into whether it forces a render and whether it forces a module-VM rebuild.
    fn classify(&self, template_extension: &str) -> (bool, bool) {
        match self {
            HydrationEvent::Template(notify::Event {
                kind:
                    notify::EventKind::Modify(
                        notify::event::ModifyKind::Name(..) | notify::event::ModifyKind::Data(..),
                    )
                    | notify::EventKind::Create(notify::event::CreateKind::File)
                    | notify::EventKind::Remove(notify::event::RemoveKind::File),
                paths: path_list,
                ..
            }) => (Self::matches(path_list, template_extension), false),
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
                let target_change = Self::matches(path_list, "luau");

                (target_change, target_change)
            }
            HydrationEvent::Context => (true, false),
            _ => (false, false),
        }
    }

    /// Determine whether every path in an event has the supplied extension.
    ///
    /// Extension matching is case insensitive. An empty path list returns `true`.
    fn matches(path_list: &[PathBuf], target_extension: &str) -> bool {
        path_list.iter().all(|target_path| {
            target_path
                .extension()
                .is_some_and(|target_value| target_value.eq_ignore_ascii_case(target_extension))
        })
    }
}

/// Template discovery and loader paths rooted in a directory.
// NOTE(invariant): None.
#[derive(Debug, Clone)]
struct TemplateTree {
    /// Directory containing source templates and generated outputs.
    template_root: PathBuf,

    /// Filename extension used for template discovery and event filtering.
    template_extension: String,
}

impl TemplateTree {
    /// Retain the template root and filename extension.
    fn new(template_root: impl AsRef<Path>, template_extension: impl Into<String>) -> Self {
        let template_root = template_root.as_ref().to_path_buf();

        let template_extension = template_extension.into();

        Self {
            template_root,
            template_extension,
        }
    }

    /// Borrow the directory containing template inputs and staged output.
    const fn root(&self) -> &PathBuf {
        let Self { template_root, .. } = self;

        template_root
    }

    /// Borrow the filename extension used to identify templates.
    fn extension(&self) -> &str {
        let Self {
            template_extension, ..
        } = self;

        template_extension
    }

    /// Build a template loader rooted at the template directory that admits dot paths.
    ///
    /// Names are checked by [`Self::join`] before reading. Filesystem reads follow symbolic links.
    fn loader(
        &self,
    ) -> impl Fn(&str) -> Result<Option<String>, minijinja::Error> + Send + Sync + 'static {
        let template_tree = self.clone();

        move |target_name| {
            let Some(target_path) = template_tree.join(target_name) else {
                return Ok(None);
            };

            match fs::read_to_string(target_path) {
                Ok(target_source) => Ok(Some(target_source)),
                Err(target_error) if target_error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(None)
                }
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
    fn join(&self, target_name: &str) -> Option<PathBuf> {
        let Self { template_root, .. } = self;

        let mut target_path = template_root.clone();

        for target_segment in target_name.split('/') {
            if target_segment.is_empty() || target_segment == ".." || target_segment.contains('\\')
            {
                return None;
            }

            target_path.push(target_segment);
        }

        Some(target_path)
    }

    /// Enumerate the loader-relative names of every template candidate under the template root.
    fn names(&self) -> eyre::Result<Vec<String>> {
        let Self {
            template_root,
            template_extension,
        } = self;

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
}

/// Rendering inputs borrowed for a pass through the template tree.
// NOTE(invariant): None.
#[derive(Debug)]
struct Renderer<'render> {
    /// Runtime used by registered asynchronous Luau callables.
    target_runtime: &'render Arc<Runtime>,

    /// Evaluated module exports available during this pass.
    lua_runtime: Option<&'render LuaRuntime>,

    /// Source template tree and output naming rules.
    template_tree: &'render TemplateTree,

    /// Handling of undefined values in the template environment.
    undefined: UndefinedKind,
}

impl<'render> Renderer<'render> {
    /// Borrow the runtime, module exports, and template settings for a render pass.
    const fn new(
        target_runtime: &'render Arc<Runtime>,
        lua_runtime: Option<&'render LuaRuntime>,
        template_tree: &'render TemplateTree,
        undefined: UndefinedKind,
    ) -> Self {
        Self {
            target_runtime,
            lua_runtime,
            template_tree,
            undefined,
        }
    }

    /// Construct a fresh [`Environment`] over the current template tree and module registrations.
    ///
    /// Each pass loads current template contents and registers exports from the retained Luau VM.
    /// Registered callables capture the worker runtime to drive asynchronous module functions.
    fn environment(&self) -> Environment<'static> {
        let Self {
            target_runtime,
            lua_runtime,
            template_tree,
            undefined,
        } = self;

        let mut target_environment = Environment::new();

        // NOTE: The loader accepts dotfiles and owns its root for the environment's `'static` lifetime.
        target_environment.set_loader(template_tree.loader());

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

    /// Stage every rendered template and return the pending output renames.
    ///
    /// An error during rendering or staging leaves destination files untouched. Each staged file
    /// receives its source template's permissions.
    fn render(
        &self,
        target_context: &toml::Value,
        temp_filedir: TempDir,
    ) -> eyre::Result<StagedRender> {
        let Self { template_tree, .. } = self;

        let target_environment = self.environment();

        let target_context = MirageValue::from(target_context).value();

        let mut rename_list = Vec::new();

        for template_name in template_tree.names()? {
            let mut template_path = template_tree.root().join(template_name.as_str());

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

        Ok(StagedRender {
            temp_filedir,
            rename_list,
        })
    }
}

/// Rendered files retained in their staging directory until output replacement or drop.
// NOTE(invariant): Every source path in `rename_list` belongs to `temp_filedir`.
#[derive(Debug)]
struct StagedRender {
    /// Directory retaining the staged files for the lifetime of this pass.
    temp_filedir: TempDir,

    /// Source and destination paths for individual output replacements.
    rename_list: Vec<(PathBuf, PathBuf)>,
}

impl StagedRender {
    /// Replace destination files with the staged renders in order.
    ///
    /// Each successful rename atomically replaces its destination on the same filesystem. A failed
    /// rename returns an error, leaving earlier replacements in place and later outputs unchanged.
    fn commit(self) -> eyre::Result<()> {
        let Self {
            temp_filedir: _temp_filedir,
            rename_list,
        } = self;

        for (from, to) in rename_list {
            fs::rename(&from, &to)
                .wrap_err_with(|| format!("could not replace output `{}`", to.display()))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Renderer, StagedRender, TemplateTree};

    use std::sync::Arc;

    use crate::manifest::UndefinedKind;

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

        let template_tree = TemplateTree::new(target_dir.path(), "jinja");

        target_environment.set_loader(template_tree.loader());

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

        let template_tree = TemplateTree::new(target_dir.path(), "jinja");

        let mut target_names = template_tree.names().unwrap();

        target_names.sort();

        assert_eq!(target_names, [".bashrc.jinja", ".config/app.conf.jinja"]);
    }

    #[test]
    fn safe_join_permits_dots_but_refuses_traversal() {
        let target_root = Path::new("/srv/templates");

        let template_tree = TemplateTree::new(target_root, "jinja");

        assert_eq!(
            template_tree.join(".config/app.conf.jinja"),
            Some(target_root.join(".config/app.conf.jinja"))
        );

        assert!(template_tree.join("../escape").is_none());
        assert!(template_tree.join("nested/../escape").is_none());
    }

    #[test]
    fn staged_outputs_outlive_the_renderer_until_commit() {
        let target_dir = tempfile::tempdir().unwrap();

        let template_path = target_dir.path().join("app.conf.jinja");
        let output_path = target_dir.path().join("app.conf");

        std::fs::write(template_path, "{{ name }}").unwrap();

        let staged = {
            let target_runtime = Arc::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            );

            let template_tree = TemplateTree::new(target_dir.path(), "jinja");

            let renderer =
                Renderer::new(&target_runtime, None, &template_tree, UndefinedKind::Strict);

            let context = toml::from_str::<toml::Value>("name = 'mirage'").unwrap();

            let temp_filedir = tempfile::tempdir_in(target_dir.path()).unwrap();

            renderer.render(&context, temp_filedir).unwrap()
        };

        assert!(!output_path.exists());

        staged.commit().unwrap();

        assert_eq!(std::fs::read_to_string(output_path).unwrap(), "mirage");
    }

    #[test]
    fn commit() {
        let directory = tempfile::tempdir().unwrap();

        let temp_filedir = tempfile::tempdir_in(directory.path()).unwrap();

        let first = directory.path().join("first");
        let blocked = directory.path().join("blocked");
        let later = directory.path().join("later");

        std::fs::write(&first, "previous").unwrap();
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(&later, "previous").unwrap();

        let mut rename_list = Vec::new();

        for (name, destination) in [("first", &first), ("blocked", &blocked), ("later", &later)] {
            let source = temp_filedir.path().join(name);

            std::fs::write(&source, "updated").unwrap();

            rename_list.push((source, destination.clone()));
        }

        let staged = StagedRender {
            temp_filedir,
            rename_list,
        };

        assert!(staged.commit().is_err());
        assert_eq!(std::fs::read_to_string(first).unwrap(), "updated");
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_to_string(later).unwrap(), "previous");
    }
}
