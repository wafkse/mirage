//! Server module.

use std::{env::home_dir, path::PathBuf};

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

use crate::{
    background::Background,
    manifest::{Manifest, ManifestConfigure},
    state::{
        configure::{ConfigurePipe, ConfigureState},
        control::ControlState,
        hydrate::HydrationState,
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
}

/// The primary state of a Mirage process.
///
/// This operates a daemon dynamically hydrates a set of Tera-based templates employing a set of "configure candidate" files.
///
/// Every file in a root template directory with a `tera` extension is rendered on each configure candidate change to the same directory modulo the "tera" extension.
///
/// This keeps logic simple and predictable, without requiring an input-output directory split or other methodology.
#[derive(Debug)]
pub struct Mirage {
    /// The state for configure candidates.
    configure_state: ConfigureState,

    /// The hydration state of the process.
    hydrate_state: HydrationState,

    /// The control state for this Mirage process.
    control_state: ControlState,
}

impl Mirage {
    /// The basename of the manifest dotfile.
    pub const MIRAGE_MANIFEST_DOTFILE: &str = ".mirage.toml";

    /// The basename of the unix domain socket dotfile.
    pub const MIRAGE_SOCK_DOTFILE: &str = ".mirage.sock";

    /// Construct a new daemon state from the target settings.
    #[inline]
    pub fn new(target_settings: MirageCli) -> eyre::Result<Self> {
        let MirageCli {
            configure: command_configure,
            profile: command_profile,
            template_root: command_template,
            listen_sock: command_sock,
            ..
        } = &target_settings;

        let Manifest {
            configure:
                ManifestConfigure {
                    template: manifest_template,
                    listen_sock: manifest_sock,
                    profile: manifest_profile,
                },
            candidate: candidate_list,
        } = Figment::new()
            .merge(Toml::file(
                command_configure.join(Self::MIRAGE_MANIFEST_DOTFILE),
            ))
            // NOTE: List-concat semantic for environment-based overrides.
            .admerge(Env::prefixed("MIRAGE_MANIFEST_"))
            .extract::<Manifest>()?;

        let template_root = command_template
            .as_ref()
            .or(manifest_template.as_ref())
            .ok_or_else(|| eyre!("no template dir provided"))?;

        let configure_state = ConfigureState::new(
            (command_configure.as_path(), candidate_list),
            command_profile
                .as_ref()
                .or(manifest_profile.as_ref())
                .as_deref()
                .map(String::as_str)
                .map(Profile::new)
                .unwrap_or(Profile::Default),
        )?;

        let hydrate_state =
            HydrationState::new(template_root, configure_state.pipe().context().subscribe())?;

        let control_socket = UnixListener::bind(
            if let Some(sock_name) = command_sock.as_ref().or(manifest_sock.as_ref()) {
                sock_name.to_path_buf()
            } else {
                home_dir()
                    .map(|home| home.join(Self::MIRAGE_SOCK_DOTFILE))
                    .ok_or_eyre("could not get default socket path")?
            },
        )?;

        let control_state =
            ControlState::new(control_socket, ConfigurePipe::clone(configure_state.pipe()));

        Ok(Self {
            configure_state,
            hydrate_state,
            control_state,
        })
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
