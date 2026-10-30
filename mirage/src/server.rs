//! Configure and run Mirage as a daemon or render templates once.
//!
//! Use [`Mirage::new`] for continuous updates or [`Mirage::oneshot`] for a synchronous render.

use std::{env::home_dir, path::PathBuf, time::Duration};

use clap::Parser;

use eyre::{OptionExt, eyre};

use figment::{
    Figment, Profile,
    providers::{Env, Format, Toml},
};

use tokio::{
    net::UnixListener,
    signal::{
        self,
        unix::{SignalKind, signal},
    },
    task::JoinSet,
};

use tracing::Instrument;

use crate::{
    background::Background,
    filesystem::ExpandedPath,
    manifest::{self, Manifest, ManifestCandidate, ManifestConfigure, UndefinedKind},
    state::{
        configure::{ConfigurePipe, ConfigureState},
        control::ControlState,
        hydrate::{HydrationState, ModuleSource},
    },
};

/// Command-line settings for daemon execution or one-shot rendering.
#[derive(Debug, Clone, Parser)]
pub struct MirageCli {
    /// The path to the directory of configure candidates.
    #[arg(short = 'C', long)]
    pub configure: PathBuf,

    /// The profile selected for the context.
    #[arg(short = 'P', long)]
    pub profile: Option<String>,

    /// The path to the Unix Domain Socket to listen to for daemon control.
    #[arg(short = 'L', long = "listen")]
    pub listen_sock: Option<PathBuf>,

    /// The path to the template directory.
    #[arg(short = 'T', long = "template", required = false)]
    pub template_root: Option<PathBuf>,

    /// Render the templates once and exit.
    #[arg(long)]
    pub oneshot: bool,
}

/// Settings shared by daemon execution and one-shot rendering.
///
/// Paths have undergone shell expansion. Command-line values take precedence over manifest
/// settings, and rendering options carry their defaults.
// NOTE(invariant): None.
#[derive(Debug)]
struct Resolved {
    /// The expanded base directory for configure candidates and relative entry-module imports.
    configure_root: PathBuf,

    /// The manifest candidate list folded into the render context.
    candidate_list: Vec<ManifestCandidate>,

    /// The selected profile.
    profile: Profile,

    /// The expanded template root.
    template_root: PathBuf,

    /// The extension borne by template candidates.
    template_extension: String,

    /// The undefined-reference behaviour for the rendering engine.
    undefined: UndefinedKind,

    /// The grace period over which filesystem notifications are coalesced.
    notificate_period: Duration,

    /// The paths and library permissions used to load the optional Luau module.
    module_source: Option<ModuleSource>,

    /// The expanded control socket path. Absence selects the default under the home directory.
    listen_sock: Option<PathBuf>,
}

impl Resolved {
    /// Resolve the command line, manifest, and defaults into [`Resolved`] settings.
    ///
    /// Command-line values override the manifest. Environment overrides for the manifest use
    /// the `MIRAGE_MANIFEST_` prefix.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be loaded, a path fails to expand, or no template directory is provided.
    fn new(target_settings: MirageCli) -> eyre::Result<Self> {
        let MirageCli {
            configure: command_configure,
            profile: command_profile,
            template_root: command_template,
            listen_sock: command_sock,
            ..
        } = target_settings;

        let configure_root = ExpandedPath::new(command_configure)?.path();

        let Manifest {
            configure:
                ManifestConfigure {
                    template: manifest_template,
                    listen_sock: manifest_sock,
                    profile: manifest_profile,
                    module: manifest_module_path,
                    template_extension: manifest_extension,
                    undefined: manifest_undefined,
                    notificate_period: manifest_period,
                },
            candidate: candidate_list,
            module: manifest_module,
        } = Figment::new()
            .merge(Toml::file(
                configure_root.join(Mirage::MIRAGE_MANIFEST_DOTFILE),
            ))
            // NOTE: Manifest environment overrides concatenate arrays through `Figment::admerge`.
            .admerge(Env::prefixed("MIRAGE_MANIFEST_"))
            .extract::<Manifest>()?;

        let template_root = ExpandedPath::new(
            command_template
                .or(manifest_template)
                .ok_or_else(|| eyre!("no template dir provided"))?,
        )?
        .path();

        let profile = command_profile
            .or(manifest_profile)
            .map_or(Profile::Default, |target_profile| {
                Profile::new(target_profile.as_str())
            });

        let module_source = manifest_module_path
            .map(|module_path| -> eyre::Result<ModuleSource> {
                let configure_root = configure_root.clone();

                let module_path = ExpandedPath::new(module_path)?.path();

                let manifest = manifest_module.clone();

                Ok(ModuleSource {
                    configure_root,
                    module_path,
                    manifest,
                })
            })
            .transpose()?;

        let listen_sock = command_sock
            .or(manifest_sock)
            .map(ExpandedPath::new)
            .transpose()?
            .map(ExpandedPath::path);

        let template_extension =
            manifest_extension.unwrap_or_else(|| manifest::DEFAULT_TEMPLATE_EXTENSION.to_string());

        let undefined = manifest_undefined.unwrap_or_default();

        let notificate_period =
            Duration::from_millis(manifest_period.unwrap_or(manifest::DEFAULT_NOTIFICATE_PERIOD));

        Ok(Self {
            configure_root,
            candidate_list,
            profile,
            template_root,
            template_extension,
            undefined,
            notificate_period,
            module_source,
            listen_sock,
        })
    }
}

/// A daemon that keeps generated configuration up to date as its inputs change.
///
/// The control socket accepts requests to select profiles, mutate context values, inspect the
/// context, and request rendering.
// NOTE(invariant): None.
#[derive(Debug)]
pub struct Mirage {
    /// The state for configure candidates.
    configure: ConfigureState,

    /// The hydration state of the process.
    hydrate: HydrationState,

    /// The control state for this Mirage process.
    control: ControlState,
}

impl Mirage {
    /// The basename of the manifest dotfile.
    pub const MIRAGE_MANIFEST_DOTFILE: &str = ".mirage.toml";

    /// The basename of the Unix domain socket dotfile.
    pub const MIRAGE_SOCK_DOTFILE: &str = ".mirage.sock";

    /// Construct a new daemon state from the target settings.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime with I/O enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be loaded, a path fails to expand, the module fails to evaluate, or the
    /// configure inputs cannot be loaded, watchers cannot be registered, or the control socket cannot be bound.
    #[inline]
    pub fn new(target_settings: MirageCli) -> eyre::Result<Self> {
        let Resolved {
            configure_root,
            candidate_list,
            profile,
            template_root,
            template_extension,
            undefined,
            notificate_period,
            module_source,
            listen_sock,
        } = Resolved::new(target_settings)?;

        let configure_state =
            ConfigureState::new((configure_root.as_path(), candidate_list), profile)?;

        let hydrate_state = HydrationState::new(
            template_root,
            configure_state.pipe().context().subscribe(),
            template_extension,
            undefined,
            notificate_period,
            module_source,
        )?;

        let control_socket = UnixListener::bind(match listen_sock {
            Some(sock_name) => sock_name,
            None => home_dir()
                .map(|home| home.join(Self::MIRAGE_SOCK_DOTFILE))
                .ok_or_eyre("could not get default socket path")?,
        })?;

        let control_state =
            ControlState::new(control_socket, ConfigurePipe::clone(configure_state.pipe()));

        Ok(Self {
            configure: configure_state,
            hydrate: hydrate_state,
            control: control_state,
        })
    }

    /// Resolve the manifest and render the templates once.
    ///
    /// # Panics
    ///
    /// Panics when rendering calls a Luau function from within an active Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be loaded, a path fails to expand, the module fails to evaluate, or the
    /// render itself fails.
    #[inline]
    pub fn oneshot(target_settings: MirageCli) -> eyre::Result<()> {
        let Resolved {
            configure_root,
            candidate_list,
            profile,
            template_root,
            template_extension,
            undefined,
            module_source,
            ..
        } = Resolved::new(target_settings)?;

        let lua_runtime = module_source.as_ref().map(ModuleSource::load).transpose()?;

        let target_context =
            ConfigureState::fold(configure_root.as_path(), &candidate_list, &profile)?;

        HydrationState::oneshot(
            lua_runtime.as_ref(),
            template_root.as_path(),
            template_extension.as_str(),
            undefined,
            &target_context,
        )
    }
}

impl Background for Mirage {
    type Output = ();

    #[inline]
    async fn run(self) -> eyre::Result<Self::Output> {
        let Self {
            configure,
            hydrate,
            control,
        } = self;

        tracing::info!("starting mirage daemon");

        let mut thread_set = JoinSet::new();

        let _ = thread_set
            .spawn(Background::run(configure).instrument(tracing::info_span!("configure")));
        let _ = thread_set.spawn(Background::run(hydrate));
        let _ =
            thread_set.spawn(Background::run(control).instrument(tracing::info_span!("control")));

        let mut terminate_signal = signal(SignalKind::terminate())?;

        tokio::select!(
            target_result = signal::ctrl_c() => target_result.map_err(eyre::Report::from),
            Some(()) = terminate_signal.recv() => Ok(()),
            target_result = thread_set.join_next() => match target_result {
                Some(target_result) => target_result?,
                None => Ok(()),
            },
        )
    }
}
