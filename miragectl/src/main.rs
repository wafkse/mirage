//! Send JSON control requests to a Mirage daemon over a Unix socket.

use std::{
    env::home_dir,
    io::{self, Write},
    path::PathBuf,
};

use clap::Parser;

use eyre::OptionExt;
use mirage::{server::Mirage, state::control::ControlRequest};
use tempfile::TempDir;
use tokio::net::UnixDatagram;

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

    /// Merge a transient TOML fragment into the daemon's render context.
    Mutate { value: String },
}

/// Socket selection and command for a control client invocation.
// NOTE(invariant): None.
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
    let connect_path = if let Some(connect_path) = connect_path {
        connect_path
    } else {
        home_dir()
            .map(|home| home.join(Mirage::MIRAGE_SOCK_DOTFILE))
            .ok_or_eyre("could not get default socket path")?
    };

    let socket_dir = TempDir::new()?;

    let socket = UnixDatagram::bind(socket_dir.path().join(RECEIVE_SOCKET_NAME))?;

    socket.connect(connect_path)?;

    let ref control_request = match control_command {
        ControlCommand::Ping => ControlRequest::Ping,
        ControlCommand::Hydrate => ControlRequest::Hydrate,
        ControlCommand::Profile { name } => ControlRequest::Profile {
            name: figment::Profile::from(name),
        },
        ControlCommand::Mutate { value } => ControlRequest::Mutate {
            context: serde_json::from_str(value.as_str())?,
        },
    };

    socket
        .send(serde_json::to_string(control_request)?.as_bytes())
        .await?;

    let mut recv_buf = bytes::BytesMut::new();

    let byte_count = socket.recv_buf(&mut recv_buf).await?;

    io::stdout().lock().write_all(&recv_buf[..byte_count])?;

    Ok(())
}
