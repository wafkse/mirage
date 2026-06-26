//! Server module.

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
    filesystem,
    manifest::{self, Manifest, ManifestCandidate, ManifestConfigure, ManifestModule, UndefinedKind},
    state::{
        configure::{self, ConfigurePipe, ConfigureState},
        control::ControlState,
        hydrate::{self, HydrationState, ModuleSource},
    },
};

/// The Mirage command-line daemon.
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

    /// Render the manifest a single time and exit, without starting the daemon.
    #[arg(long)]
    pub oneshot: bool,
}

/// The settings resolved out of the command line and manifest, shared by the daemon and the one-shot render.
///
/// Every path here has already been shell expanded and the optional manifest keys folded against their defaults, so both
/// the reactive [`Mirage::new`] and the synchronous [`Mirage::render_oneshot`] consume an identical, fully-resolved view.
#[derive(Debug)]
struct Resolved {
    /// The expanded configure root, doubling as the candidate root and the `require` jail.
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

    /// The Luau module provenance, when a module is configured.
    module_source: Option<ModuleSource>,

    /// The expanded control socket path, when one is given; the daemon falls back to a home-relative default.
    listen_sock: Option<PathBuf>,
}

/// Resolve the command line and manifest into the fully-expanded [`Resolved`] settings.
///
/// Every path input — the `-C`/`-T`/`-L` flags and the manifest `template`/`listen_sock`/`module` keys — is shell
/// expanded once here at load time; template *content* is deliberately never expanded.
///
/// # Errors
///
/// Returns an error when the manifest cannot be loaded, a path fails to expand, or no template directory is provided.
fn resolve(target_settings: MirageCli) -> eyre::Result<Resolved> {
    let MirageCli {
        configure: command_configure,
        profile: command_profile,
        template_root: command_template,
        listen_sock: command_sock,
        ..
    } = target_settings;

    // NOTE: Expand the configure root first; the manifest and candidate tree all resolve relative to it.
    let configure_root = filesystem::shell_expand(command_configure)?;

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
        // NOTE: List-concat semantic for environment-based overrides.
        .admerge(Env::prefixed("MIRAGE_MANIFEST_"))
        .extract::<Manifest>()?;

    let template_root = filesystem::shell_expand(
        command_template
            .or(manifest_template)
            .ok_or_else(|| eyre!("no template dir provided"))?,
    )?;

    let profile = command_profile
        .or(manifest_profile)
        .map_or(Profile::Default, |target_profile| {
            Profile::new(target_profile.as_str())
        });

    let module_source = manifest_module_path
        .map(|module_path| -> eyre::Result<ModuleSource> {
            Ok(ModuleSource {
                configure_root: configure_root.clone(),
                module_path: filesystem::shell_expand(module_path)?,
                manifest: ManifestModule::clone(&manifest_module),
            })
        })
        .transpose()?;

    let listen_sock = command_sock
        .or(manifest_sock)
        .map(filesystem::shell_expand)
        .transpose()?;

    Ok(Resolved {
        configure_root,
        candidate_list,
        profile,
        template_root,
        template_extension: manifest_extension
            .unwrap_or_else(|| manifest::DEFAULT_TEMPLATE_EXTENSION.to_string()),
        undefined: manifest_undefined.unwrap_or_default(),
        notificate_period: Duration::from_millis(
            manifest_period.unwrap_or(manifest::DEFAULT_NOTIFICATE_PERIOD),
        ),
        module_source,
        listen_sock,
    })
}

/// The primary state of a Mirage process.
///
/// This operates a daemon dynamically hydrates a set of minijinja templates employing a set of "configure candidate" files.
///
/// Every file in a root template directory with the configured template extension is rendered on each configure candidate change to the same directory modulo that extension.
///
/// This keeps logic simple and predictable, without requiring an input-output directory split or other methodology.
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

    /// The basename of the unix domain socket dotfile.
    pub const MIRAGE_SOCK_DOTFILE: &str = ".mirage.sock";

    /// Construct a new daemon state from the target settings.
    ///
    /// This binds the control socket and wires up the watchers, so it must run within a Tokio runtime context.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be loaded, a path fails to expand, the module fails to evaluate, or the
    /// control socket cannot be bound.
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
        } = resolve(target_settings)?;

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

    /// Render the manifest a single time and exit, without starting the daemon.
    ///
    /// This folds the configure context, builds the Luau runtime once, and performs one all-or-nothing render. No
    /// watchers are established and no control socket is bound, so it neither needs nor enters a Tokio runtime — the
    /// one-shot render owns its own current-thread runtime internally to drive any async module code.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be loaded, a path fails to expand, the module fails to evaluate, or the
    /// render itself fails.
    #[inline]
    pub fn render_oneshot(target_settings: MirageCli) -> eyre::Result<()> {
        let Resolved {
            configure_root,
            candidate_list,
            profile,
            template_root,
            template_extension,
            undefined,
            module_source,
            ..
        } = resolve(target_settings)?;

        let lua_runtime = module_source
            .as_ref()
            .map(ModuleSource::load)
            .transpose()?;

        let target_context =
            configure::fold_context(configure_root.as_path(), &candidate_list, &profile)?;

        hydrate::render_once(
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

    async fn run(self) -> eyre::Result<Self::Output> {
        let Self {
            configure_state,
            hydrate_state,
            control_state,
        } = self;

        let mut thread_set = JoinSet::new();

        let _ = thread_set.spawn(Background::run(configure_state));
        let _ = thread_set.spawn(Background::run(hydrate_state));
        let _ = thread_set.spawn(Background::run(control_state));

        let mut terminate_signal = signal(SignalKind::terminate())?;

        let target_value = 'a: {
            tokio::select!(
                // NOTE: The nest level of inner breakable blocks emitted by the macro is unknown, so the label is required.
                Ok(..) = signal::ctrl_c() => break 'a Ok(()),
                Some(..) = terminate_signal.recv() => break 'a Ok(()),
                target_value = thread_set.join_all() => {
                    for target_value in target_value {
                        let _ = target_value?;
                    }

                    Ok(())
                },
            )
        };

        target_value
    }
}
