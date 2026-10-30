//! Template rendering and Luau extension reloads.
//!
//! See [`HydrationState`] for additional information.

use std::{
    collections::BTreeSet,
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

/// Paths and library permissions for a Luau extension module.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    /// The configure root, used as the base for entry imports and filesystem reads.
    pub configure_root: PathBuf,

    /// The module path, resolved relative to the configure root.
    pub module_path: PathBuf,

    /// The `[module]` manifest section gating host libraries.
    pub manifest: ManifestModule,
}

/// Render templates in response to changes in templates, Luau modules, and the TOML context.
///
/// A failed module reload leaves the previous extensions available for subsequent rendering.
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
    /// Watch the configured inputs for template rendering.
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

    /// Render the initial context and process subsequent input changes.
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

    #[inline]
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

    /// Render every template once and replace the generated outputs.
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

    /// Log a recoverable rendering or module-reload error.
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

        target_list.sort();

        Ok(target_list)
    }
}

/// A discovered template bound to a context and an output path relative to the root.
#[derive(Debug)]
// NOTE(invariant): None.
struct TemplateInstance<'context> {
    /// Template name relative to the discovered template root.
    name: String,

    /// Expanded output path relative to the template root.
    destination: PathBuf,

    /// Borrowed context selected by the template path parameters.
    context: &'context toml::Value,
}

impl TemplateTree {
    /// Expand whole-component `{path}` parameters, looking up dotted paths in the current context.
    /// Later parameters descend from the previously selected child. Missing/non-table paths
    /// produce no instances. No schema is imposed on the selected child values.
    fn instances<'context>(
        &self,
        context: &'context toml::Value,
    ) -> eyre::Result<Vec<TemplateInstance<'context>>> {
        let mut instances = Vec::new();
        let mut destinations = BTreeSet::new();
        let names = self.names()?;

        for name in &names {
            let mut paths = vec![(PathBuf::new(), context)];
            let output = Path::new(name).with_extension("");
            for component in output.components() {
                let component = component.as_os_str().to_string_lossy();
                let parameter = component
                    .strip_prefix('{')
                    .and_then(|s| s.strip_suffix('}'));
                let mut expanded = Vec::new();
                for (path, context) in paths {
                    if let Some(parameter) = parameter {
                        let selected = parameter
                            .split('.')
                            .try_fold(context, |value, key| value.get(key));
                        if let Some(table) = selected.and_then(toml::Value::as_table) {
                            let mut children: Vec<_> = table.iter().collect();
                            children.sort_by(|a, b| a.0.cmp(b.0));
                            for (key, value) in children {
                                if !Self::safe_component(key) {
                                    return Err(eyre!("invalid path binding `{key}` in `{name}`"));
                                }
                                expanded.push((path.join(key), value));
                            }
                        }
                    } else {
                        expanded.push((path.join(component.as_ref()), context));
                    }
                }
                paths = expanded;
            }
            for (destination, context) in paths {
                if !destinations.insert(destination.clone()) {
                    return Err(eyre!(
                        "duplicate output destination `{}`",
                        destination.display()
                    ));
                }
                instances.push(TemplateInstance {
                    name: name.clone(),
                    destination,
                    context,
                });
            }
        }

        let root = self.root().canonicalize()?;
        let mut resolved_destinations = BTreeSet::new();
        for destination in &destinations {
            // NOTE: Resolve existing parent directories, including internal symlink aliases.
            // Rename replaces a destination symlink itself, so the final component is not canonicalized.
            let output = root.join(destination);
            let ancestor = output
                .ancestors()
                .skip(1)
                .find(|p| p.exists())
                .ok_or_else(|| eyre!("output has no existing parent"))?;
            let resolved_parent = ancestor.canonicalize()?;
            if !resolved_parent.starts_with(&root) {
                return Err(eyre!(
                    "output escapes template root: `{}`",
                    destination.display()
                ));
            }
            let resolved = resolved_parent.join(output.strip_prefix(ancestor)?);
            if !resolved_destinations.insert(resolved) {
                return Err(eyre!(
                    "duplicate output destination `{}`",
                    destination.display()
                ));
            }
        }
        for destination in &resolved_destinations {
            // NOTE: Reject file/directory conflicts and outputs that would overwrite template inputs.
            if destination
                .ancestors()
                .skip(1)
                .any(|p| resolved_destinations.contains(p))
                || names.iter().any(|name| root.join(name) == *destination)
            {
                return Err(eyre!(
                    "conflicting output destination `{}`",
                    destination.display()
                ));
            }
        }
        Ok(instances)
    }

    /// Accept a nonempty filename component without traversal or platform separators.
    fn safe_component(value: &str) -> bool {
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains(['/', '\\', ':', '\0'])
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

    /// Create a template environment with the current Luau exports.
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

        let instances = template_tree.instances(target_context)?;
        let mut rename_list = Vec::new();

        for TemplateInstance {
            name,
            destination,
            context,
        } in instances
        {
            let template_path = template_tree.root().join(&name);
            let template_perms = fs::metadata(&template_path)?.permissions();
            let temp_path = temp_filedir.path().join(&destination);
            fs::create_dir_all(
                temp_path
                    .parent()
                    .ok_or_else(|| eyre!("tempfile has no parent"))?,
            )?;

            let rendered_output = target_environment
                .get_template(&name)?
                .render(MirageValue::from(context).value())?;
            fs::write(&temp_path, rendered_output)?;
            fs::set_permissions(&temp_path, template_perms)?;
            tracing::trace!(template = %name, "staged template");
            rename_list.push((temp_path, template_tree.root().join(destination)));
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
            fs::create_dir_all(to.parent().ok_or_else(|| eyre!("output has no parent"))?)?;
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

    fn render(root: &Path, context: &str) -> eyre::Result<()> {
        super::HydrationState::oneshot(
            None,
            root,
            "jinja",
            UndefinedKind::Strict,
            &toml::from_str(context)?,
        )
    }

    #[test]
    fn parameters_bind_subcontexts_and_preserve_ordinary_templates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("post/{post}")).unwrap();
        std::fs::write(root.join("index.jinja"), "{{ site }}:{{ post|length }}").unwrap();
        std::fs::write(
            root.join("post/{post}/index.html.jinja"),
            "{{ title }}:{{ repository|default('none') }}",
        )
        .unwrap();
        render(root, "site = 'Example'\n[post.hello]\ntitle = 'Hello'\n[post.mirage]\ntitle = 'Mirage'\nrepository = 'url'").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("index")).unwrap(),
            "Example:2"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("post/hello/index.html")).unwrap(),
            "Hello:none"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("post/mirage/index.html")).unwrap(),
            "Mirage:url"
        );

        // Deleted keys leave prior outputs in place; new instances still render.
        render(root, "site = 'New'\n[post.hello]\ntitle = 'Updated'").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("post/mirage/index.html")).unwrap(),
            "Mirage:url"
        );
    }

    #[test]
    fn invalid_bindings_and_collisions_leave_outputs_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("{post}")).unwrap();
        std::fs::write(root.join("{post}/index.jinja"), "{{ title }}").unwrap();
        for key in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "a/b",
            "a\\b",
            "C:drive",
        ] {
            let mut children = toml::Table::new();
            children.insert(key.into(), toml::Value::Table(toml::Table::new()));
            let context = toml::Value::Table(toml::Table::from_iter([(
                "post".into(),
                toml::Value::Table(children),
            )]));
            assert!(
                TemplateTree::new(root, "jinja")
                    .instances(&context)
                    .is_err(),
                "{key:?}"
            );
        }
        std::fs::create_dir_all(root.join("hello")).unwrap();
        std::fs::write(root.join("hello/index.jinja"), "ordinary").unwrap();
        std::fs::write(root.join("hello/index"), "previous").unwrap();
        assert!(
            render(root, "[post.hello]\ntitle = 'new'")
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("hello/index")).unwrap(),
            "previous"
        );
    }

    #[test]
    fn output_file_directory_conflicts_and_template_overwrites_are_rejected() {
        for template in ["hello.jinja", "hello/index.jinja"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::write(root.join("{post}.jinja"), "generated").unwrap();
            std::fs::create_dir_all(root.join(template).parent().unwrap()).unwrap();
            std::fs::write(root.join(template), "original").unwrap();
            // First case would overwrite hello.jinja; second would make hello both a file and directory.
            let context = if template == "hello.jinja" {
                "[post.'hello.jinja']"
            } else {
                "[post.hello]"
            };
            assert!(
                render(root, context)
                    .unwrap_err()
                    .to_string()
                    .contains("conflicting")
            );
            assert_eq!(
                std::fs::read_to_string(root.join(template)).unwrap(),
                "original"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_destinations_collide() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("{post}")).unwrap();
        std::fs::create_dir(root.join("actual")).unwrap();
        std::os::unix::fs::symlink(root.join("actual"), root.join("alias")).unwrap();
        std::fs::write(root.join("{post}/index.jinja"), "new").unwrap();
        assert!(
            render(root, "[post.actual]\n[post.alias]")
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        assert!(!root.join("actual/index").exists());
    }

    #[test]
    fn parameter_render_failure_preserves_all_previous_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("{post}")).unwrap();
        std::fs::write(root.join("{post}/index.jinja"), "{{ title }}").unwrap();
        render(root, "[post.a]\ntitle = 'old a'\n[post.b]\ntitle = 'old b'").unwrap();
        assert!(render(root, "[post.a]\ntitle = 'new a'\n[post.b]\nother = true").is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("a/index")).unwrap(),
            "old a"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b/index")).unwrap(),
            "old b"
        );
    }

    #[test]
    fn nested_parameters_and_missing_collections_expand_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("{group}/{items}")).unwrap();
        std::fs::write(dir.path().join("{group}/{items}/out.jinja"), "{{ value }}").unwrap();
        let tree = TemplateTree::new(dir.path(), "jinja");
        let context =
            toml::from_str("[group.z.items.b]\nvalue = 2\n[group.a.items.a]\nvalue = 1").unwrap();
        let paths: Vec<_> = tree
            .instances(&context)
            .unwrap()
            .into_iter()
            .map(|i| i.destination)
            .collect();
        assert_eq!(paths, [Path::new("a/a/out"), Path::new("z/b/out")]);
        assert!(
            tree.instances(&toml::Value::Table(toml::Table::new()))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn generated_outputs_cannot_escape_through_symlink_directories() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("{post}")).unwrap();
        std::fs::write(dir.path().join("{post}/index.jinja"), "new").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        assert!(render(dir.path(), "[post.escape]").is_err());
        assert!(!outside.path().join("index").exists());
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
