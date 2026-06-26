//! The `miragectl` CLI utility to interact with an online `miraged` server.

use std::{
    env::home_dir,
    io::{self, Write},
    path::PathBuf,
};

use bytes::Bytes;
use clap::Parser;

use eyre::OptionExt;

use futures::sink::SinkExt;
use futures::stream::StreamExt;

use mirage::{server::Mirage, state::control::ControlRequest};

use tokio::net::UnixStream;

use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// The name of the receive end socket for a `miragectl` instance.
pub const RECEIVE_SOCKET_NAME: &str = "miragectl.sock";

/// A daemon control command.
#[derive(Debug, Parser, Clone)]
pub enum ControlCommand {
    /// Send a ping message to the daemon to determine responsiveness.
    Ping,

    /// Trigger a re-hydrate on the daemon.
    Hydrate,

    /// Send a profile-change message to the daemon.
    Profile {
        /// The name of the profile to use.
        name: String,
    },

    /// Mutate the context used in the daemon for hydrate processes in a transient manner.
    Mutate { value: String },

    /// Tell the daemon to dump its complete context to us.
    Context,
}

/// A command-line utility to control an online Mirage Daemon.
#[derive(Debug, Parser)]
struct MirageCtlCli {
    /// The remote daemon endpoint to connect to.
    #[arg(short = 'C', long = "connect")]
    connect_path: Option<PathBuf>,

    /// The control command to send to the daemon.
    #[clap(subcommand)]
    control_command: ControlCommand,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let MirageCtlCli {
        connect_path,
        control_command,
        ..
    } = MirageCtlCli::parse();

    // FIXME: Deduplicate this logic between server and control client.
    let target_sock = if let Some(target_sock) = connect_path {
        target_sock
    } else {
        home_dir()
            .map(|home| home.join(Mirage::MIRAGE_SOCK_DOTFILE))
            .ok_or_eyre("could not get default socket path")?
    };

    let mut target_endpoint = Framed::new(
        UnixStream::connect(target_sock).await?,
        LengthDelimitedCodec::new(),
    );

    let control_request = &match control_command {
        ControlCommand::Ping => ControlRequest::Ping,
        ControlCommand::Hydrate => ControlRequest::Hydrate,
        ControlCommand::Profile { name } => ControlRequest::Profile {
            name: figment::Profile::from(name),
        },
        ControlCommand::Mutate { value } => ControlRequest::Mutate { expression: value },
        ControlCommand::Context => ControlRequest::Context,
    };

    target_endpoint
        .send(Bytes::copy_from_slice(
            serde_json::to_string(control_request)?.as_bytes(),
        ))
        .await?;

    if let Some(target_value) = target_endpoint.next().await {
        io::stdout()
            .lock()
            .write_all(target_value?.iter().as_slice())?;
    }

    Ok(())
}
