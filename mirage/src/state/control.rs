//! Control state management.

use futures::{SinkExt, StreamExt};
use tokio::{
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::{mpsc, oneshot},
};

use bytes::Bytes;

use serde::{Deserialize, Serialize};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::{background::Background, state::configure::ConfigurePipe};

/// A control message request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "lowercase")]
pub enum ControlRequest {
    /// A ping message.
    Ping,

    /// A profile change message.
    Profile {
        /// The name of the profile to change to.
        name: figment::Profile,
    },

    /// A hydrate message.
    ///
    /// This forces a full re-hydrate operation.
    Hydrate,

    /// A mutate message.
    ///
    /// This deep-merges the provided TOML fragment on top of the already-established render context.
    ///
    /// This is ephemeral and does not persist across re-hydrate operations.
    Mutate {
        /// The raw TOML fragment to deep-merge into the source context (e.g. `system.battery-state = "low"`).
        expression: String,
    },

    /// A context message.
    ///
    /// This permits dumping the context of the daemon to a requesting client.
    Context,
}

/// A control message response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "lowercase")]
pub enum ControlResponse {
    /// A pong message.
    Pong,

    /// An acknowledgement message.
    ///
    /// This is used for side-effectful control requests to indicate that the request was sucessfully processed.
    Acknowledge,

    /// A context message.
    ///
    /// This is used to dump the complete context of the daemon.
    Context {
        /// The associated context.
        context: serde_json::Value,
    },

    /// An error occurred while processing the control request.
    Error {
        /// The error message.
        message: String,
    },
}

/// Deep-merge an overlay value tree onto a base render context.
///
/// Tables merge recursively, while arrays and scalars replace wholesale. A type mismatch between corresponding keys also
/// resolves in favour of the overlay.
pub fn deep_merge(target_context: &mut toml::Value, override_context: toml::Value) {
    match (target_context, override_context) {
        (toml::Value::Table(target_table), toml::Value::Table(override_table)) => {
            for (target_key, override_value) in override_table {
                match target_table.get_mut(&target_key) {
                    Some(target_value) => deep_merge(target_value, override_value),
                    None => {
                        let _ = target_table.insert(target_key, override_value);
                    }
                }
            }
        }
        (target_context, override_context) => *target_context = override_context,
    }
}

/// The control state.
#[derive(Debug)]
pub struct ControlState(UnixListener, ConfigurePipe);

impl ControlState {
    /// Instantiate a control state.
    #[inline]
    pub const fn new(control_listener: UnixListener, configure_pipe: ConfigurePipe) -> Self {
        Self(control_listener, configure_pipe)
    }

    /// Handle a control message.
    fn handle(
        ConfigurePipe(context_pipe, profile_pipe): &mut ConfigurePipe,
        target_request: ControlRequest,
    ) -> eyre::Result<ControlResponse> {
        match target_request {
            ControlRequest::Ping => {
                tracing::debug!("handling ping");

                Ok::<_, eyre::Error>(ControlResponse::Pong)
            }
            ControlRequest::Profile { name } => {
                tracing::info!(profile = %name, "switching profile");

                profile_pipe.send(name)?;

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Hydrate => {
                tracing::info!("forcing re-hydrate");

                // NOTE: This fakes out a mutation, which triggers a re-hydrate without us having to re-create the context.
                context_pipe.send_modify(|_| ());

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Mutate { expression } => {
                tracing::info!(%expression, "mutating context");

                // NOTE: Parse with `toml_edit` so dotted-key fragments and primitive typing are honoured.
                let override_context =
                    toml_edit::de::from_str::<toml::Value>(expression.as_str())
                        .map_err(eyre::Report::from)?;

                context_pipe
                    .send_modify(|target_context| deep_merge(target_context, override_context));

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Context => {
                tracing::debug!("dumping context");

                // NOTE: The wire encoding is JSON; the internal model stays a `toml::Value`.
                let context = serde_json::to_value(context_pipe.borrow().clone())?;

                Ok(ControlResponse::Context { context })
            }
        }
    }
}

impl Background for ControlState {
    type Output = ();

    async fn run(mut self) -> eyre::Result<Self::Output> {
        enum Operation {
            Socket(UnixStream),
            Request(
                (
                    ControlRequest,
                    oneshot::Sender<eyre::Result<ControlResponse>>,
                ),
            ),
        }

        let Self(ref mut target_endpoint, ref mut target_pipe) = self;

        let (ref state_send, mut state_recv) = mpsc::unbounded_channel::<(
            ControlRequest,
            oneshot::Sender<eyre::Result<ControlResponse>>,
        )>();

        loop {
            let target_operate = tokio::select!(
               Ok((target_left, ..)) = target_endpoint.accept() => Operation::Socket(target_left),
               Some(target_tuple) = state_recv.recv() => {
                  Operation::Request(target_tuple)
              }
            );

            match target_operate {
                Operation::Socket(target_io) => {
                    let control_endpoint = mpsc::UnboundedSender::clone(state_send);

                    let mut framed_endpoint = Framed::new(target_io, LengthDelimitedCodec::new());

                    let target_handle = async move {
                        while let Some(Ok(target_value)) = framed_endpoint.next().await {
                            let target_response = match serde_json::from_slice::<ControlRequest>(
                                target_value.iter().as_slice(),
                            )
                            .map_err(eyre::Report::from)
                            {
                                Ok(target_value) => {
                                    let (target_left, target_response) = oneshot::channel();

                                    match control_endpoint
                                        .send((target_value, target_left))
                                        .map_err(eyre::Report::from)
                                    {
                                        Ok(..) => target_response
                                            .await
                                            .map_err(eyre::Report::from)
                                            .flatten(),
                                        Err(target_error) => Err(target_error),
                                    }
                                }
                                Err(target_error) => Err(target_error),
                            };

                            let target_response = &match target_response {
                                Ok(target_value) => target_value,
                                Err(target_error) => ControlResponse::Error {
                                    message: target_error.to_string(),
                                },
                            };

                            if let Ok(target_value) = serde_json::to_string(target_response) {
                                let _ = framed_endpoint
                                    .send(Bytes::copy_from_slice(target_value.as_bytes()))
                                    .await;
                            }
                        }
                    };

                    tokio::spawn(target_handle);
                }
                Operation::Request((target_message, target_channel)) => {
                    let _ = target_channel.send(ControlState::handle(target_pipe, target_message));
                }
            }
        }
    }
}

impl Drop for ControlState {
    fn drop(&mut self) {
        let Self(target_endpoint, ..) = self;

        if let Some(local_file) = target_endpoint
            .local_addr()
            .as_ref()
            .map(SocketAddr::as_pathname)
            .ok()
            .flatten()
        {
            let _ = std::fs::remove_file(local_file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::deep_merge;

    #[test]
    fn tables_merge_while_arrays_and_scalars_replace() {
        let mut target_context = toml::from_str::<toml::Value>(
            "[system]\nbattery = \"full\"\nload = 1\ntags = [\"a\"]\n",
        )
        .unwrap();

        let override_context =
            toml::from_str::<toml::Value>("[system]\nbattery = \"low\"\ntags = [\"b\", \"c\"]\n")
                .unwrap();

        deep_merge(&mut target_context, override_context);

        let target_system = target_context.get("system").unwrap();

        // The overlaid scalar replaces, the untouched scalar survives, and the array replaces wholesale.
        assert_eq!(target_system.get("battery").unwrap().as_str(), Some("low"));
        assert_eq!(target_system.get("load").unwrap().as_integer(), Some(1));
        assert_eq!(
            target_system.get("tags").unwrap().as_array().map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn dotted_fragment_parses_and_merges() {
        let mut target_context =
            toml::from_str::<toml::Value>("[system]\nbattery = \"full\"\n").unwrap();

        let override_context =
            toml_edit::de::from_str::<toml::Value>("system.battery-state = \"low\"").unwrap();

        deep_merge(&mut target_context, override_context);

        let target_system = target_context.get("system").unwrap();

        assert_eq!(target_system.get("battery").unwrap().as_str(), Some("full"));
        assert_eq!(
            target_system.get("battery-state").unwrap().as_str(),
            Some("low")
        );
    }
}
